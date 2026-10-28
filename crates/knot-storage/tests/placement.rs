//! Placement: where `create_placed`, `move_placed` and restore put a page
//! among its siblings, how a group's keys are respread when no key fits, how
//! placements serialize with each other, and how moves keep out of, and
//! cope with, parent cycles, against a fresh database per test.
//!
//! The concurrency tests order their steps with a stand-in transaction on a
//! connection of its own, and wait until the sessions under test are blocked
//! on a lock before taking the next step, so each runs the same interleaving
//! every time.

use std::collections::HashMap;

use knot_storage::{
    DocStore, DocStoreError, PgDocStore, PgUserStore, PgWorkspaceStore, Placement, UserStore,
    WorkspaceRole, WorkspaceStore, lexorank,
};
use knot_test_support::locks::{TOP_LEVEL_LOCK, deadlocks_once, wait_for_lock_waits};
use sqlx::PgPool;
use uuid::Uuid;

struct World {
    store: PgDocStore,
    pool: PgPool,
    ws: Uuid,
    user: Uuid,
}

async fn world() -> World {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let user = PgUserStore::new(pool.clone())
        .create_local("a@x.test", "A", "$h$")
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, user.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    World {
        store: PgDocStore::new(pool.clone()),
        pool,
        ws: ws.id,
        user: user.id,
    }
}

impl World {
    /// A page inserted with raw SQL, so its key can be one the generator
    /// never writes (as an old release or the database before the heal could).
    async fn seed(&self, parent: Option<Uuid>, title: &str, key: &str) -> Uuid {
        self.seed_id(Uuid::new_v4(), parent, title, key).await
    }

    /// As `seed`, with a chosen id, for tests that depend on the order rows
    /// are locked in.
    async fn seed_id(&self, id: Uuid, parent: Option<Uuid>, title: &str, key: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO documents (id, workspace_id, parent_id, title, sort_key, created_by)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(id)
        .bind(self.ws)
        .bind(parent)
        .bind(title)
        .bind(key)
        .bind(self.user)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// A page at the end of the top level, placed the new way.
    async fn page(&self, title: &str) -> Uuid {
        self.store
            .create_placed(self.ws, None, title, Placement::Last, self.user)
            .await
            .unwrap()
            .id
    }

    /// A page at the end of `parent`'s children, placed the new way.
    async fn child(&self, parent: Uuid, title: &str) -> Uuid {
        self.store
            .create_placed(self.ws, Some(parent), title, Placement::Last, self.user)
            .await
            .unwrap()
            .id
    }

    /// The whole group under `parent` (`None`: the top level), archived rows
    /// and templates included, as `(title, sort_key)` in the sidebar's order:
    /// by code unit.
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

    async fn parent_of(&self, id: Uuid) -> Option<Uuid> {
        self.store.get(id).await.unwrap().unwrap().parent_id
    }
}

/// The titles of `group`, after asserting that every key is valid, at most
/// 64 characters long, and unique.
fn healthy_titles(group: &[(String, String)]) -> Vec<&str> {
    for (title, key) in group {
        assert!(
            lexorank::is_valid(key) && key.len() <= 64,
            "{title} has the bad key {key:?}: {group:?}"
        );
    }
    assert!(
        group
            .windows(2)
            .all(|w| w[0].1.as_bytes() < w[1].1.as_bytes()),
        "duplicate keys: {group:?}"
    );
    group.iter().map(|(title, _)| title.as_str()).collect()
}

/// Keys from before the heal or from an old release, in the sidebar's order:
/// below the alphabet (`"0"`), a trailing '0', upper case, and over 64
/// characters. `between` rejects the first four as neighbours, and the next
/// key after the last would be 71 characters long.
fn bad_keys() -> [(&'static str, String); 5] {
    [
        ("zero", "0".to_string()),
        ("double zero", "00".to_string()),
        ("upper M", "M".to_string()),
        ("m zero", "m0".to_string()),
        ("long", "z".repeat(70)),
    ]
}

/// Creates and moves next to each kind of bad key respread the group and
/// then succeed. Every key ends up valid, short and unique, the seeded pages
/// keep their order, and the page lands where it was asked to.
#[tokio::test(flavor = "multi_thread")]
async fn placement_rebalances_invalid_neighbours() {
    #[derive(Clone, Copy, Debug)]
    enum Op {
        /// Create "new" here.
        Create(Anchor),
        /// Move a top-level page, "new", into the group here.
        MoveIn(Anchor),
        /// Move the seeded page with this title within the group.
        MoveOwn(&'static str, Anchor),
    }
    #[derive(Clone, Copy, Debug)]
    enum Anchor {
        First,
        Last,
        After(&'static str),
        Before(&'static str),
    }
    let cases: [(Op, &[&str]); 8] = [
        (
            Op::Create(Anchor::First),
            &["new", "zero", "double zero", "upper M", "m zero", "long"],
        ),
        (
            Op::Create(Anchor::After("zero")),
            &["zero", "new", "double zero", "upper M", "m zero", "long"],
        ),
        (
            Op::Create(Anchor::After("m zero")),
            &["zero", "double zero", "upper M", "m zero", "new", "long"],
        ),
        (
            Op::Create(Anchor::Last),
            &["zero", "double zero", "upper M", "m zero", "long", "new"],
        ),
        (
            Op::MoveIn(Anchor::Before("upper M")),
            &["zero", "double zero", "new", "upper M", "m zero", "long"],
        ),
        (
            Op::MoveIn(Anchor::After("long")),
            &["zero", "double zero", "upper M", "m zero", "long", "new"],
        ),
        (
            Op::MoveOwn("zero", Anchor::After("long")),
            &["double zero", "upper M", "m zero", "long", "zero"],
        ),
        (
            Op::MoveOwn("long", Anchor::First),
            &["long", "zero", "double zero", "upper M", "m zero"],
        ),
    ];
    let w = world().await;
    for (op, expected) in cases {
        let parent = w.page("Parent").await;
        let mut ids = HashMap::new();
        for (title, key) in bad_keys() {
            ids.insert(title, w.seed(Some(parent), title, &key).await);
        }
        let placement = |anchor| match anchor {
            Anchor::First => Placement::First,
            Anchor::Last => Placement::Last,
            Anchor::After(title) => Placement::After(ids[title]),
            Anchor::Before(title) => Placement::Before(ids[title]),
        };
        let result = match op {
            Op::Create(anchor) => w
                .store
                .create_placed(w.ws, Some(parent), "new", placement(anchor), w.user)
                .await
                .map(drop),
            Op::MoveIn(anchor) => {
                let mover = w.page("new").await;
                w.store
                    .move_placed(w.ws, mover, w.user, Some(parent), placement(anchor))
                    .await
                    .map(drop)
            }
            Op::MoveOwn(title, anchor) => w
                .store
                .move_placed(w.ws, ids[title], w.user, Some(parent), placement(anchor))
                .await
                .map(drop),
        };
        result.unwrap_or_else(|e| panic!("{op:?}: {e:?}"));
        let group = w.group(Some(parent)).await;
        assert_eq!(healthy_titles(&group), expected, "{op:?}");
    }
}

/// Twenty creates at the top of a group at once, under a parent and at the
/// top level: each holds the group's lock while it reads the first key and
/// inserts, so none reuses another's key.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_first_creates_get_distinct_keys() {
    let w = world().await;
    let parent = w.page("Parent").await;
    for group in [Some(parent), None] {
        let creates: Vec<_> = (0..20)
            .map(|i| {
                let store = w.store.clone();
                let (ws, user) = (w.ws, w.user);
                tokio::spawn(async move {
                    store
                        .create_placed(ws, group, &format!("Page {i}"), Placement::First, user)
                        .await
                })
            })
            .collect();
        for create in creates {
            create.await.unwrap().unwrap();
        }
        let titles = w.group(group).await;
        let pages = healthy_titles(&titles)
            .into_iter()
            .filter(|t| t.starts_with("Page "))
            .count();
        assert_eq!(pages, 20, "{group:?}: {titles:?}");
    }
}

/// A respread of a group while one of its pages moves to another parent.
/// The move holds the page's row lock, so the respread's single UPDATE waits
/// for it; once the move commits, the UPDATE's parent condition skips the
/// page, which is no longer in the group. Overwriting its key would collide:
/// the other parent's pages hold exactly the keys the respread hands out.
#[tokio::test(flavor = "multi_thread")]
async fn rebalance_concurrent_with_sibling_move_both_succeed() {
    let w = world().await;
    let group_parent = w.page("Group").await;
    // The move locks the moved page and its new parent in id order: the page
    // first, then the parent the stand-in below holds.
    let sibling = Uuid::from_u128(1);
    let other_parent = Uuid::from_u128(u128::MAX);
    w.seed(Some(group_parent), "zero", "0").await;
    w.seed_id(sibling, Some(group_parent), "sibling", "c").await;
    w.seed(Some(group_parent), "i", "i").await;
    w.seed(Some(group_parent), "o", "o").await;
    w.seed_id(other_parent, None, "Other", "t").await;
    for (n, key) in lexorank::spread(4).iter().enumerate() {
        w.seed(Some(other_parent), &format!("other {n}"), key).await;
    }

    let mut holder = w.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM documents WHERE id = $1 FOR SHARE")
        .bind(other_parent)
        .execute(&mut *holder)
        .await
        .unwrap();
    let mover = {
        let (store, ws, user) = (w.store.clone(), w.ws, w.user);
        tokio::spawn(async move {
            store
                .move_placed(ws, sibling, user, Some(other_parent), Placement::Last)
                .await
        })
    };
    // The move has locked the sibling and waits for its new parent.
    wait_for_lock_waits(&w.pool, 1).await;
    let creator = {
        let (store, ws, user) = (w.store.clone(), w.ws, w.user);
        tokio::spawn(async move {
            store
                .create_placed(ws, Some(group_parent), "new", Placement::First, user)
                .await
        })
    };
    // The create's respread waits for the sibling's row.
    wait_for_lock_waits(&w.pool, 2).await;
    holder.rollback().await.unwrap();

    mover.await.unwrap().expect("the move");
    creator.await.unwrap().expect("the create next to \"0\"");
    assert_eq!(w.parent_of(sibling).await, Some(other_parent));
    let group = w.group(Some(group_parent)).await;
    assert_eq!(healthy_titles(&group), ["new", "zero", "i", "o"]);
    let other = w.group(Some(other_parent)).await;
    assert_eq!(
        healthy_titles(&other),
        ["other 0", "other 1", "other 2", "other 3", "sibling"]
    );
}

/// Two moves that swap nesting, D into P and P into D. Each locks both rows
/// before its cycle check, so the second waits for the first to commit, then
/// sees D under P and refuses with `Cycle`. Checking before locking let both
/// succeed, leaving D and P each other's parent.
#[tokio::test(flavor = "multi_thread")]
async fn swapping_nesting_moves_do_not_deadlock() {
    let w = world().await;
    let d = w.page("D").await;
    let p = w.page("P").await;
    // Holds the first move just before it commits, with its row locks taken:
    // its last write is the ACL invalidation.
    let mut holder = w.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE acl_invalidations IN EXCLUSIVE MODE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let first = {
        let (store, ws, user) = (w.store.clone(), w.ws, w.user);
        tokio::spawn(async move {
            store
                .move_placed(ws, d, user, Some(p), Placement::Last)
                .await
        })
    };
    wait_for_lock_waits(&w.pool, 1).await;
    let second = {
        let (store, ws, user) = (w.store.clone(), w.ws, w.user);
        tokio::spawn(async move {
            store
                .move_placed(ws, p, user, Some(d), Placement::Last)
                .await
        })
    };
    wait_for_lock_waits(&w.pool, 2).await;
    holder.commit().await.unwrap();

    first.await.unwrap().expect("D into P");
    let err = second
        .await
        .unwrap()
        .expect_err("P into D after D went under P");
    assert!(matches!(err, DocStoreError::Cycle), "got {err:?}");
    assert_eq!(w.parent_of(d).await, Some(p));
    assert_eq!(w.parent_of(p).await, None);
}

/// Two moves on disjoint rows that close a parent cycle together: C is B's
/// parent and A is D's, then A goes into B while C goes into D. Each locked
/// only its own two rows, so both cycle checks read the tree before either
/// move committed, both passed, and both committed: A → B → C → D → A, four
/// pages the sidebar can't place. Moves into a parent now take a lock of
/// their own per workspace first, so the second check sees the first move
/// and refuses with `Cycle`.
#[tokio::test(flavor = "multi_thread")]
async fn disjoint_moves_do_not_close_a_parent_cycle() {
    let w = world().await;
    let c = w.page("C").await;
    let b = w.child(c, "B").await;
    let a = w.page("A").await;
    let d = w.child(a, "D").await;
    // Holds the first move just before it commits, with its row locks taken:
    // its last write is the ACL invalidation.
    let mut holder = w.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE acl_invalidations IN EXCLUSIVE MODE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let move_into = |doc: Uuid, parent: Uuid| {
        let (store, ws, user) = (w.store.clone(), w.ws, w.user);
        tokio::spawn(async move {
            store
                .move_placed(ws, doc, user, Some(parent), Placement::Last)
                .await
        })
    };
    let first = move_into(a, b);
    wait_for_lock_waits(&w.pool, 1).await;
    let second = move_into(c, d);
    wait_for_lock_waits(&w.pool, 2).await;
    holder.commit().await.unwrap();

    first.await.unwrap().expect("A into B");
    let err = second
        .await
        .unwrap()
        .expect_err("C into D after A went into B");
    assert!(matches!(err, DocStoreError::Cycle), "got {err:?}");
    let parents = [
        w.parent_of(a).await,
        w.parent_of(b).await,
        w.parent_of(c).await,
        w.parent_of(d).await,
    ];
    assert_eq!(
        parents,
        [Some(b), Some(c), None, Some(a)],
        "A, B, C, D: C stays at the top level"
    );
}

/// A store on this test's database whose statements Postgres cancels after
/// five seconds, so that a query that never ends fails the test instead of
/// running until the server is killed. Nothing else holds a lock here, and
/// every statement takes milliseconds.
fn store_with_statement_timeout(w: &World) -> PgDocStore {
    let options = (*w.pool.connect_options())
        .clone()
        .options([("statement_timeout", "5s")]);
    PgDocStore::new(
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy_with(options),
    )
}

/// A parent cycle already in the database: `A` under `B`, `B` under `A`,
/// both live. Two racing moves could commit one (above), and so could
/// releases before the moves took their locks.
async fn seed_cycle(w: &World, name: &str) -> (Uuid, Uuid) {
    let a = w.seed(None, &format!("{name} A"), name).await;
    let b = w.seed(Some(a), &format!("{name} B"), "m").await;
    sqlx::query("UPDATE documents SET parent_id = $2 WHERE id = $1")
        .bind(a)
        .bind(b)
        .execute(&w.pool)
        .await
        .unwrap();
    (a, b)
}

/// The cycle check walks down from the moved page. From a page of a cycle,
/// its `UNION ALL` walk never ended, so moving that page anywhere outside
/// the cycle ran until the server was killed, holding its row locks and a
/// pool connection. It finishes now, and the move breaks the cycle. A page
/// moved into the cycle was never affected (its walk stays below it); it
/// still goes in.
#[tokio::test(flavor = "multi_thread")]
async fn moves_into_and_out_of_a_parent_cycle_finish() {
    let w = world().await;
    let store = store_with_statement_timeout(&w);
    let (a, b) = seed_cycle(&w, "c").await;
    let page = w.page("Page").await;
    let outside = w.page("Outside").await;

    store
        .move_placed(w.ws, page, w.user, Some(a), Placement::Last)
        .await
        .expect("a page into the cycle");
    store
        .move_placed(w.ws, a, w.user, Some(outside), Placement::Last)
        .await
        .expect("a page of the cycle out of it");

    let parents = [
        w.parent_of(a).await,
        w.parent_of(b).await,
        w.parent_of(page).await,
    ];
    assert_eq!(parents, [Some(outside), Some(a), Some(a)], "A, B, Page");
}

/// Archive stamps the subtree it walks down from the archived page with
/// `UNION ALL`, which never ended on a cycle. It finishes now and archives
/// the whole cycle, with what hangs below it, under the page archived.
#[tokio::test(flavor = "multi_thread")]
async fn archiving_a_page_of_a_parent_cycle_finishes() {
    let w = world().await;
    let store = store_with_statement_timeout(&w);
    let (a, b) = seed_cycle(&w, "c").await;
    let below = w.child(a, "Below").await;

    let archived = store
        .archive(w.ws, b, w.user)
        .await
        .expect("archive a page of the cycle");

    assert_eq!(archived.first(), Some(&b), "the archived page first");
    let mut archived = archived;
    archived.sort();
    let mut expected = vec![a, b, below];
    expected.sort();
    assert_eq!(archived, expected);
}

/// The subpages of a page in a cycle, which the ACL listener lists after an
/// archive or a grant change, came from a `UNION ALL` walk that never ended
/// either. Now they are every page below it, the page itself left out.
#[tokio::test(flavor = "multi_thread")]
async fn subpages_of_a_page_in_a_parent_cycle_are_listed() {
    let w = world().await;
    let store = store_with_statement_timeout(&w);
    let (a, b) = seed_cycle(&w, "c").await;
    let below = w.child(a, "Below").await;

    let mut subpages = store
        .descendant_ids(a)
        .await
        .expect("the subpages of a page of the cycle");

    subpages.sort();
    let mut expected = vec![b, below];
    expected.sort();
    assert_eq!(subpages, expected);
}

/// A restore to the top level (its parent is archived) while a top-level
/// rebalance holds the group's lock. Restore takes that lock before it locks
/// the parent, so the rebalance can still rewrite the parent's key (the
/// archived parent is in the top-level group). Locking the parent first and
/// the group second, inside `place`, deadlocked against it.
#[tokio::test(flavor = "multi_thread")]
async fn restore_concurrent_with_top_level_rebalance_both_succeed() {
    let w = world().await;
    let parent = w.page("Parent").await;
    w.page("Other").await;
    let child = w
        .store
        .create_placed(w.ws, Some(parent), "Child", Placement::Last, w.user)
        .await
        .unwrap()
        .id;
    w.store.archive(w.ws, child, w.user).await.unwrap();
    w.store.archive(w.ws, parent, w.user).await.unwrap();

    // Stands in for a top-level rebalance that holds the group's lock.
    let mut rebalance = w.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2::uuid::text))")
        .bind(TOP_LEVEL_LOCK)
        .bind(w.ws)
        .execute(&mut *rebalance)
        .await
        .unwrap();
    let restore = {
        let (store, ws, user) = (w.store.clone(), w.ws, w.user);
        tokio::spawn(async move { store.restore(ws, child, user).await })
    };
    wait_for_lock_waits(&w.pool, 1).await;
    // The rebalance rewrites every top-level key, the archived parent's too.
    sqlx::query("SELECT 1 FROM documents WHERE id = $1 FOR NO KEY UPDATE NOWAIT")
        .bind(parent)
        .execute(&mut *rebalance)
        .await
        .expect("restore must not hold the parent while it waits for the top-level lock");
    sqlx::query("UPDATE documents SET sort_key = 'a' WHERE id = $1")
        .bind(parent)
        .execute(&mut *rebalance)
        .await
        .unwrap();
    rebalance.commit().await.unwrap();

    restore.await.unwrap().expect("the restore");
    assert_eq!(w.parent_of(child).await, None, "its parent is archived");
    let top = w.group(None).await;
    assert_eq!(healthy_titles(&top), ["Parent", "Other", "Child"]);
}

/// An anchor must be a live page under the target parent, not a template,
/// and not the page being moved. Anything else is refused with `BadAnchor`
/// and changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn anchors_must_be_live_non_template_siblings() {
    let w = world().await;
    let live = w.page("Live").await;
    let archived = w.page("Archived").await;
    w.store.archive(w.ws, archived, w.user).await.unwrap();
    let template = w.page("Template").await;
    w.store
        .set_template(w.ws, template, w.user, true)
        .await
        .unwrap();
    let folder = w.page("Folder").await;
    let nested = w
        .store
        .create_placed(w.ws, Some(folder), "Nested", Placement::Last, w.user)
        .await
        .unwrap()
        .id;
    let mover = w.page("Mover").await;
    let before = w.group(None).await;

    let bad = [archived, template, nested, Uuid::new_v4()];
    for anchor in bad {
        for placement in [Placement::After(anchor), Placement::Before(anchor)] {
            let create = w
                .store
                .create_placed(w.ws, None, "New", placement, w.user)
                .await;
            assert!(
                matches!(create, Err(DocStoreError::BadAnchor)),
                "create {placement:?}: {create:?}"
            );
        }
    }
    for anchor in bad.into_iter().chain([mover]) {
        for placement in [Placement::After(anchor), Placement::Before(anchor)] {
            let moved = w
                .store
                .move_placed(w.ws, mover, w.user, None, placement)
                .await;
            assert!(
                matches!(moved, Err(DocStoreError::BadAnchor)),
                "move {placement:?}: {moved:?}"
            );
        }
    }
    assert_eq!(w.group(None).await, before, "nothing changed");

    w.store
        .move_placed(w.ws, mover, w.user, None, Placement::Before(live))
        .await
        .expect("a live sibling is an anchor");
    let created = w
        .store
        .create_placed(
            w.ws,
            Some(folder),
            "Second",
            Placement::After(nested),
            w.user,
        )
        .await
        .expect("so is a live page under the target parent");
    assert_eq!(created.parent_id, Some(folder));
}

#[tokio::test(flavor = "multi_thread")]
async fn create_placed_retries_once_after_a_deadlock() {
    let w = world().await;
    let blocker = w.seed(None, "zero", "0").await;
    let (store, ws, user) = (w.store.clone(), w.ws, w.user);
    deadlocks_once(&w.pool, w.ws, blocker, async move {
        store
            .create_placed(ws, None, "New", Placement::First, user)
            .await
    })
    .await;
    assert_eq!(healthy_titles(&w.group(None).await), ["New", "zero"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn move_placed_retries_once_after_a_deadlock() {
    let w = world().await;
    let blocker = w.seed(None, "zero", "0").await;
    let folder = w.seed(None, "Folder", "m").await;
    let mover = w.seed(Some(folder), "Mover", "m").await;
    let (store, ws, user) = (w.store.clone(), w.ws, w.user);
    deadlocks_once(&w.pool, w.ws, blocker, async move {
        store
            .move_placed(ws, mover, user, None, Placement::First)
            .await
    })
    .await;
    assert_eq!(
        healthy_titles(&w.group(None).await),
        ["Mover", "zero", "Folder"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_retries_once_after_a_deadlock() {
    let w = world().await;
    let parent = w.seed(None, "Parent", "m").await;
    let child = w.seed(Some(parent), "Child", "m").await;
    w.store.archive(w.ws, child, w.user).await.unwrap();
    w.store.archive(w.ws, parent, w.user).await.unwrap();
    // Last at the top level is after this key, which `between` rejects.
    let blocker = w.seed(None, "z zero", "z0").await;
    let (store, ws, user) = (w.store.clone(), w.ws, w.user);
    deadlocks_once(&w.pool, w.ws, blocker, async move {
        store.restore(ws, child, user).await
    })
    .await;
    assert_eq!(
        healthy_titles(&w.group(None).await),
        ["Parent", "z zero", "Child"]
    );
}

/// A restore to the top level (its parent is archived) when the last
/// top-level key is one an old release could write and `between` rejects.
/// The group is respread, and the page goes last with a valid key. Before
/// placement, restore took the top level's largest key and passed it to
/// `sort_key_between`, which panicked.
#[tokio::test(flavor = "multi_thread")]
async fn restore_next_to_an_invalid_last_key_goes_last() {
    let w = world().await;
    let parent = w.seed(None, "Parent", "m").await;
    let child = w.seed(Some(parent), "Child", "m").await;
    w.store.archive(w.ws, child, w.user).await.unwrap();
    w.store.archive(w.ws, parent, w.user).await.unwrap();
    // The last top-level key ends in '0', which `between` rejects.
    w.seed(None, "z zero", "z0").await;

    let restored = w
        .store
        .restore(w.ws, child, w.user)
        .await
        .expect("the restore");
    assert_eq!(restored, [child]);
    assert_eq!(w.parent_of(child).await, None, "its parent is archived");
    assert_eq!(
        healthy_titles(&w.group(None).await),
        ["Parent", "z zero", "Child"]
    );
}

/// The schema doesn't keep a parent in its child's workspace. Restore reads
/// the parent wherever it is, so a page whose parent is archived in another
/// workspace still goes to the top level of its own, instead of coming back
/// live under an archived parent, where purging that parent deletes it.
#[tokio::test(flavor = "multi_thread")]
async fn restore_under_a_parent_archived_in_another_workspace_goes_to_the_top_level() {
    let w = world().await;
    let other_ws = PgWorkspaceStore::new(w.pool.clone())
        .create("other", "O")
        .await
        .unwrap()
        .id;
    let parent: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, parent_id, title, sort_key, created_by)
         VALUES ($1, NULL, 'Elsewhere', 'm', $2) RETURNING id",
    )
    .bind(other_ws)
    .bind(w.user)
    .fetch_one(&w.pool)
    .await
    .unwrap();
    let page = w.seed(Some(parent), "Page", "m").await;
    w.store.archive(w.ws, page, w.user).await.unwrap();
    w.store.archive(other_ws, parent, w.user).await.unwrap();

    let restored = w.store.restore(w.ws, page, w.user).await.unwrap();

    assert_eq!(restored, [page]);
    assert_eq!(w.parent_of(page).await, None, "its parent is archived");
    assert_eq!(healthy_titles(&w.group(None).await), ["Page"]);
}

/// A page of another workspace is not a parent, live or archived, and
/// neither is an id no page has: creates and moves under one fail with
/// `NotFound`, as for an unknown id, and write nothing. The foreign key only
/// proves the row exists, so they used to put the page under another
/// workspace's page (and failed on the foreign key for an unknown id). The
/// explicit-key `create` and `move_to` read the parent the same way.
#[tokio::test(flavor = "multi_thread")]
async fn writes_under_a_parent_outside_the_workspace_are_not_found() {
    let w = world().await;
    let other_ws = PgWorkspaceStore::new(w.pool.clone())
        .create("other", "O")
        .await
        .unwrap()
        .id;
    let foreign = async |title| {
        w.store
            .create_placed(other_ws, None, title, Placement::Last, w.user)
            .await
            .unwrap()
            .id
    };
    let live = foreign("Elsewhere").await;
    let archived = foreign("Archived elsewhere").await;
    w.store.archive(other_ws, archived, w.user).await.unwrap();
    let mover = w.page("Mover").await;
    let documents = async || -> Vec<(Uuid, Uuid, Option<Uuid>, String, String, String)> {
        sqlx::query_as(
            "SELECT id, workspace_id, parent_id, title, sort_key, updated_at::text
               FROM documents ORDER BY id",
        )
        .fetch_all(&w.pool)
        .await
        .unwrap()
    };
    let before = documents().await;

    let mut wrong = Vec::new();
    for (parent, name) in [
        (live, "a live page of another workspace"),
        (archived, "an archived page of another workspace"),
        (Uuid::new_v4(), "an unknown id"),
    ] {
        let results = [
            w.store
                .create_placed(w.ws, Some(parent), "Stray", Placement::Last, w.user)
                .await
                .map(drop),
            w.store
                .move_placed(w.ws, mover, w.user, Some(parent), Placement::Last)
                .await
                .map(drop),
            w.store
                .create(w.ws, Some(parent), "Stray", "m", w.user)
                .await
                .map(drop),
            w.store
                .move_to(w.ws, mover, w.user, Some(parent), "m")
                .await
                .map(drop),
        ];
        for (op, result) in ["create_placed", "move_placed", "create", "move_to"]
            .into_iter()
            .zip(results)
        {
            if !matches!(result, Err(DocStoreError::NotFound)) {
                wrong.push(format!("{op} under {name}: {result:?}"));
            }
        }
    }
    assert!(wrong.is_empty(), "not NotFound:\n{}", wrong.join("\n"));
    assert_eq!(documents().await, before, "nothing was written");
}

/// A page of another workspace under the moved page, which a create under a
/// foreign parent could leave before such creates were refused: a move into
/// it is refused as for any foreign parent, before the cycle check, whose
/// `Cycle` would tell that the page exists.
#[tokio::test(flavor = "multi_thread")]
async fn move_into_a_foreign_page_under_the_moved_page_is_not_found() {
    let w = world().await;
    let other_ws = PgWorkspaceStore::new(w.pool.clone())
        .create("other", "O")
        .await
        .unwrap()
        .id;
    let mover = w.page("Mover").await;
    let foreign_child: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, parent_id, title, sort_key, created_by)
         VALUES ($1, $2, 'Foreign child', 'm', $3) RETURNING id",
    )
    .bind(other_ws)
    .bind(mover)
    .bind(w.user)
    .fetch_one(&w.pool)
    .await
    .unwrap();

    let moved = w
        .store
        .move_placed(w.ws, mover, w.user, Some(foreign_child), Placement::Last)
        .await;

    assert!(
        matches!(moved, Err(DocStoreError::NotFound)),
        "got {moved:?}"
    );
    assert_eq!(w.parent_of(mover).await, None);
}

/// A writer that doesn't take the group's lock (an old release during a
/// rolling upgrade) takes the key restore picked, between restore's read of
/// the group and its write. Restore then fails with `Conflict`, as create
/// and move do, rather than with a database error, and restores nothing.
#[tokio::test(flavor = "multi_thread")]
async fn restore_racing_an_unlocked_writer_for_its_key_is_a_conflict() {
    let w = world().await;
    let parent = w.seed(None, "Parent", "m").await;
    let child = w.seed(Some(parent), "Child", "m").await;
    w.store.archive(w.ws, child, w.user).await.unwrap();
    w.store.archive(w.ws, parent, w.user).await.unwrap();

    // The key restore picks: last at the top level, after "m". The old pod
    // hasn't committed, so restore's read of the group doesn't see it.
    let last = lexorank::between(Some("m"), None).unwrap();
    let mut old_pod = w.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO documents (workspace_id, parent_id, title, sort_key, created_by)
         VALUES ($1, NULL, 'Old pod', $2, $3)",
    )
    .bind(w.ws)
    .bind(&last)
    .bind(w.user)
    .execute(&mut *old_pod)
    .await
    .unwrap();
    let restore = {
        let (store, ws, user) = (w.store.clone(), w.ws, w.user);
        tokio::spawn(async move { store.restore(ws, child, user).await })
    };
    // Restore's write waits for the old pod's row with the same key.
    wait_for_lock_waits(&w.pool, 1).await;
    old_pod.commit().await.unwrap();

    let err = restore
        .await
        .unwrap()
        .expect_err("the old pod took the key");
    assert!(matches!(err, DocStoreError::Conflict), "got {err:?}");
    let doc = w.store.get(child).await.unwrap().unwrap();
    assert!(doc.archived_at.is_some(), "nothing was restored");
    assert_eq!(doc.parent_id, Some(parent));
}

/// The group lock under a parent replaces `create`'s `FOR SHARE` on it and
/// still keeps creates racing an archive of the parent's folder from
/// leaving a live child under an archived parent.
#[tokio::test(flavor = "multi_thread")]
async fn placed_create_racing_archive_never_leaves_live_child() {
    let w = world().await;
    let folder = w.page("Folder").await;
    let child = w
        .store
        .create_placed(w.ws, Some(folder), "Child", Placement::Last, w.user)
        .await
        .unwrap()
        .id;

    let creates: Vec<_> = (0..20)
        .map(|_| {
            let (store, ws, user) = (w.store.clone(), w.ws, w.user);
            tokio::spawn(async move {
                store
                    .create_placed(ws, Some(child), "Racer", Placement::First, user)
                    .await
            })
        })
        .collect();
    let archiver = {
        let (store, ws, user) = (w.store.clone(), w.ws, w.user);
        tokio::spawn(async move { store.archive(ws, folder, user).await })
    };
    for create in creates {
        match create.await.unwrap() {
            Ok(_) | Err(DocStoreError::Archived) => {}
            Err(e) => panic!("unexpected create error: {e:?}"),
        }
    }
    archiver.await.unwrap().unwrap();

    let orphans: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM documents c JOIN documents p ON c.parent_id = p.id
          WHERE c.archived_at IS NULL AND p.archived_at IS NOT NULL",
    )
    .fetch_one(&w.pool)
    .await
    .unwrap();
    assert_eq!(orphans, 0, "live child left under an archived parent");
}

/// Placement reads the group by code unit, the sidebar's order, whatever
/// the column's collation: a respread under a locale (ICU here, as
/// production databases often use, where `a` sorts before `B`) keeps the
/// order users see.
#[tokio::test(flavor = "multi_thread")]
async fn rebalance_keeps_the_sidebar_order_under_a_locale_collation() {
    let w = world().await;
    sqlx::query(r#"ALTER TABLE documents ALTER COLUMN sort_key TYPE text COLLATE "und-x-icu""#)
        .execute(&w.pool)
        .await
        .unwrap();
    let parent = w.page("Parent").await;
    for key in ["m", "a", "M", "B"] {
        w.seed(Some(parent), key, key).await;
    }
    w.store
        .create_placed(w.ws, Some(parent), "new", Placement::First, w.user)
        .await
        .unwrap();
    assert_eq!(
        healthy_titles(&w.group(Some(parent)).await),
        ["new", "B", "M", "a", "m"]
    );
}

/// Both creates, placed and with an explicit key, end in `commit_create`,
/// which records one `doc.create` audit event for the new page and its
/// creator in the create's transaction.
#[tokio::test(flavor = "multi_thread")]
async fn creates_record_a_doc_create_audit_event() {
    let w = world().await;
    let placed = w.page("Placed").await;
    let explicit = w
        .store
        .create(w.ws, None, "Explicit", "t", w.user)
        .await
        .unwrap()
        .id;
    for id in [placed, explicit] {
        let events: Vec<(Uuid, Option<Uuid>, String, String)> = sqlx::query_as(
            "SELECT workspace_id, actor_id, action, target_kind FROM audit_events
              WHERE target_id = $1",
        )
        .bind(id)
        .fetch_all(&w.pool)
        .await
        .unwrap();
        assert_eq!(
            events,
            [(w.ws, Some(w.user), "doc.create".into(), "doc".into())]
        );
    }
}
