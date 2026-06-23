# Horizontally Scalable Boards Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Excalidraw boards converge across pods like documents, by wiring the existing `Bus` into `BoardRoom`.

**Architecture:** Boards already have a durable seq-ordered update log (`BoardStore`). Add a `since(id, after_seq)` query, then make `BoardRoom` publish each local update's seq (and presence) to the shared `Bus` and apply remote updates fetched via `since` — mirroring the document `Room`. Reuse the same `Bus` instance (board UUIDs are disjoint from doc UUIDs, so the per-UUID Postgres channels never collide). No schema change.

**Tech Stack:** Rust — `knot-storage` (sqlx), `knot-crdt` (tokio actors, `Bus`/`MemBus`/`PgBus`), `knot-server`. Tests: `cargo nextest run -p <crate>` against dev-compose Postgres (`knot_test_support::fresh_db`; never testcontainers). `cargo clippy -- -D warnings`.

**Spec:** `docs/superpowers/specs/2026-06-23-board-horizontal-scaling-design.md`

**Preconditions:** dev-compose Postgres healthy (`make compose.up`).

---

## File Structure

- Modify: `crates/knot-storage/src/boards.rs` — `BoardStore::since` (trait + `PgBoardStore` impl).
- Modify: `crates/knot-storage/tests/boards.rs` — `since` test.
- Modify: `crates/knot-crdt/src/board_room.rs` — bus fields, `spawn` signature, two `select!` arms, publish on inbound/awareness, watermark.
- Modify: `crates/knot-crdt/src/board_registry.rs` — `new(.., bus)`, subscribe in `acquire`, unsubscribe in `evict`.
- Modify: `crates/knot-server/src/main.rs:192` — pass the bus into `BoardRooms::new`.
- Create: `crates/knot-crdt/tests/board_fanout.rs` — two-room cross-pod convergence test.
- Modify: `README.md` + `deploy/helm/knot/values.yaml` (or its README) — note boards are now multi-pod.

---

## Task 1: `BoardStore::since`

**Files:**
- Modify: `crates/knot-storage/src/boards.rs`
- Test: `crates/knot-storage/tests/boards.rs`

Context: `BoardStore` (`boards.rs:~55`) has `append_update(id, bytes) -> Result<i64>` (`INSERT ... RETURNING seq`), `load_updates(id) -> Result<Vec<Vec<u8>>>` (`SELECT bytes ... ORDER BY seq`), and `max_update_seq(id) -> Result<i64>`. `board_updates` has columns `(board_id, seq bigserial, bytes)`. `Result<T>` is the crate's `boards::Result` alias. `tests/boards.rs` has `async fn setup() -> (PgBoardStore, Uuid /*doc_id*/, Uuid /*user*/)` (creates ws→user→doc); each test then makes a board via `store.create(doc_id, user, label)`.

- [ ] **Step 1: Write the failing test**

Append to `crates/knot-storage/tests/boards.rs`, reusing its existing `setup()`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn since_returns_updates_after_seq_in_order() {
    let (store, doc_id, user) = setup().await;
    let board_id = store.create(doc_id, user, None).await.unwrap().id;

    let s1 = store.append_update(board_id, b"u1").await.unwrap();
    let s2 = store.append_update(board_id, b"u2").await.unwrap();
    let s3 = store.append_update(board_id, b"u3").await.unwrap();

    // since(0) → all, in seq order.
    let all = store.since(board_id, 0).await.unwrap();
    assert_eq!(all.iter().map(|(_, b)| b.clone()).collect::<Vec<_>>(),
               vec![b"u1".to_vec(), b"u2".to_vec(), b"u3".to_vec()]);
    assert_eq!(all.iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![s1, s2, s3]);

    // since(s1) → only the later two.
    let rest = store.since(board_id, s1).await.unwrap();
    assert_eq!(rest.iter().map(|(_, b)| b.clone()).collect::<Vec<_>>(),
               vec![b"u2".to_vec(), b"u3".to_vec()]);

    // since(max) → none.
    assert!(store.since(board_id, s3).await.unwrap().is_empty());
}
```

If the file's `setup()` returns something different (e.g. just the store + ids), adapt the first line to match — do not invent a new fixture.

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run -p knot-storage --test boards since_returns_updates`
Expected: FAIL — `since` not a method on `BoardStore` (compile error).

- [ ] **Step 3: Add the trait method**

In `boards.rs`, in the `BoardStore` trait (next to `load_updates`):

```rust
    /// Board updates with seq > after_seq, in seq order. Used for incremental
    /// cross-pod catch-up: the bus delivers a seq, the room fetches the bytes.
    async fn since(&self, id: Uuid, after_seq: i64) -> Result<Vec<(i64, Vec<u8>)>>;
```

- [ ] **Step 4: Implement on `PgBoardStore`**

In the `impl BoardStore for PgBoardStore` block (next to `load_updates`):

```rust
    async fn since(&self, id: Uuid, after_seq: i64) -> Result<Vec<(i64, Vec<u8>)>> {
        let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT seq, bytes FROM board_updates
             WHERE board_id = $1 AND seq > $2 ORDER BY seq",
        )
        .bind(id)
        .bind(after_seq)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
```

(Match the file's existing error-mapping style if `?` doesn't directly convert `sqlx::Error` — mirror how `load_updates` maps errors.)

- [ ] **Step 5: Run to verify pass**

Run: `cargo nextest run -p knot-storage --test boards since_returns_updates`
Expected: PASS.

- [ ] **Step 6: fmt + clippy + commit**

```bash
cargo fmt -p knot-storage && cargo clippy -p knot-storage --all-targets -- -D warnings
git add crates/knot-storage/src/boards.rs crates/knot-storage/tests/boards.rs
git commit -m "feat(boards): BoardStore::since for incremental catch-up"
```

---

## Task 2: Wire the `Bus` into `BoardRoom` + registry + construction

**Files:**
- Modify: `crates/knot-crdt/src/board_room.rs`
- Modify: `crates/knot-crdt/src/board_registry.rs`
- Modify: `crates/knot-server/src/main.rs` (the `BoardRooms::new` call at ~line 192)

These land together (the signature changes must all compile). Context: the document `Room` is the reference — `Subscription { updates: mpsc::Receiver<i64>, presence: mpsc::Receiver<Vec<u8>> }` (`bus.rs`); `Room` stores `bus_updates_rx`/`bus_presence_rx` and has `select!` arms `Some(_seq) = self.bus_updates_rx.recv()` / `Some(payload) = self.bus_presence_rx.recv()` (`room.rs:452-455`); `Rooms::acquire` does `bus.subscribe(doc_id)` then passes the sub into `Room::spawn` (`registry.rs:82`); eviction does `bus.unsubscribe` (`registry.rs:126`). `BoardRoom::on_inbound` currently: `apply_update` → `store.append_update` → `wrap_sync_update` → broadcast to local conns (skipping origin).

- [ ] **Step 1: Add bus fields + receivers to `BoardRoom`**

In `board_room.rs`, add to `use`: `use crate::bus::Bus;` (and ensure `Subscription` is importable from `crate::bus`). Add fields to `struct BoardRoom`:

```rust
    bus: Arc<dyn Bus>,
    bus_updates_rx: mpsc::Receiver<i64>,
    bus_presence_rx: mpsc::Receiver<Vec<u8>>,
    last_applied_seq: i64,
```

- [ ] **Step 2: Change `spawn` to accept the bus + subscription**

Update the signature and construction:

```rust
    pub async fn spawn(
        board_id: Uuid,
        engine: Arc<dyn Engine>,
        store: Arc<dyn knot_storage::BoardStore>,
        bus: Arc<dyn Bus>,
        subscription: crate::bus::Subscription,
    ) -> Result<BoardRoomHandle, EngineError> {
```

After hydration (after the existing snapshot+update replay, before building `Self`), compute the watermark:

```rust
        let last_applied_seq = store.max_update_seq(board_id).await.unwrap_or(0);
```

and set the new fields when building `room`:

```rust
        let room = Self {
            board_id,
            engine,
            doc,
            conns: HashMap::new(),
            shutdown: shutdown.clone(),
            rx,
            store,
            bus,
            bus_updates_rx: subscription.updates,
            bus_presence_rx: subscription.presence,
            last_applied_seq,
        };
```

- [ ] **Step 3: Add the two `select!` arms**

In `run`'s `tokio::select!` (add two branches alongside the existing `msg = self.rx.recv()` one):

```rust
                Some(_seq) = self.bus_updates_rx.recv() => {
                    // Remote edit on another pod: fetch the new updates from the
                    // shared log and apply locally. These are already persisted —
                    // do NOT re-append them.
                    if let Ok(updates) = self.store.since(self.board_id, self.last_applied_seq).await {
                        for (seq, bytes) in updates {
                            if seq <= self.last_applied_seq { continue; }
                            if self.engine.apply_update(&self.doc, &bytes).is_ok() {
                                let framed = wrap_sync_update(&bytes);
                                let mut to_close: Vec<ConnId> = Vec::new();
                                for (cid, conn) in &self.conns {
                                    if conn.tx.try_send(framed.clone()).is_err() {
                                        to_close.push(*cid);
                                    }
                                }
                                for cid in to_close { self.conns.remove(&cid); }
                            }
                            self.last_applied_seq = seq;
                        }
                    }
                }
                Some(payload) = self.bus_presence_rx.recv() => {
                    let mut to_close: Vec<ConnId> = Vec::new();
                    for (cid, conn) in &self.conns {
                        if conn.tx.try_send(payload.clone()).is_err() {
                            to_close.push(*cid);
                        }
                    }
                    for cid in to_close { self.conns.remove(&cid); }
                }
```

- [ ] **Step 4: Publish local edits + presence to the bus**

In `on_inbound`, after the existing `store.append_update(...)` succeeds, capture its returned seq, advance the watermark, and publish. Change the persist block to:

```rust
        match self.store.append_update(self.board_id, &m.bytes).await {
            Ok(seq) => {
                // Advance the watermark BEFORE publishing so our own NOTIFY is a
                // no-op (since(seq) returns nothing) and we don't re-apply.
                self.last_applied_seq = seq;
                if let Err(e) = self.bus.publish(self.board_id, seq).await {
                    tracing::warn!(error=?e, "board bus publish failed");
                }
            }
            Err(e) => tracing::warn!(error=?e, "board append_update failed"),
        }
```

In the `AwarenessIn` arm, after the existing local fan-out loop, add:

```rust
                        let _ = self.bus.publish_presence(self.board_id, payload).await;
```

(Note: `payload` is moved into the loop currently via `.clone()` per-conn, so it is still owned here — confirm it is available after the loop; if the loop consumed it, clone before the loop.)

- [ ] **Step 5: Update `BoardRooms` registry**

In `board_registry.rs`: add `bus: Arc<dyn Bus>` to the `BoardRooms` struct and `new`:

```rust
    pub fn new(engine: Arc<dyn Engine>, store: Arc<dyn BoardStore>, bus: Arc<dyn crate::bus::Bus>) -> Self {
        Self { map: DashMap::new(), inflight: DashMap::new(), engine, store, bus }
    }
```

In `acquire`, subscribe and pass it through:

```rust
        let sub = self.bus.subscribe(board_id).await.expect("board bus subscribe");
        let h = BoardRoom::spawn(board_id, self.engine.clone(), self.store.clone(), self.bus.clone(), sub)
            .await
            .expect("hydrate board room");
```

In `evict`, after cancelling, unsubscribe:

```rust
    pub async fn evict(&self, board_id: Uuid) {
        if let Some((_, h)) = self.map.remove(&board_id) {
            h.shutdown.cancel();
            let _ = self.bus.unsubscribe(board_id).await;
            metrics::gauge!("knot_board_room_active").decrement(1.0);
        }
    }
```

- [ ] **Step 6: Pass the bus at construction (`main.rs:192`)**

Read `crates/knot-server/src/main.rs` around line 192 where `knot_crdt::BoardRooms::new(...)` is called. The same `Bus` the document `Rooms` use is in scope there (the constructed `PgBus`, or `state.bus`). Add it as the third argument: `knot_crdt::BoardRooms::new(engine, store, bus.clone())`. Use the exact bus binding already used for the document `Rooms` construction nearby — do not construct a second bus.

- [ ] **Step 7: Fix any existing callers of the changed signatures**

Run `cargo build -p knot-crdt -p knot-server 2>&1`. If existing tests in `board_room.rs`/`board_registry.rs` (or elsewhere) call `BoardRoom::spawn` / `BoardRooms::new` with the old arity, update them to pass a `MemBus` + subscription (for `spawn`: `let bus = Arc::new(MemBus::new()); let sub = bus.subscribe(board_id).await.unwrap();`). Fix until it compiles.

- [ ] **Step 8: Build, test, clippy**

Run: `cargo build -p knot-crdt -p knot-server && cargo nextest run -p knot-crdt && cargo clippy -p knot-crdt -p knot-server --all-targets -- -D warnings`
Expected: builds, existing tests pass, no warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/knot-crdt/src/board_room.rs crates/knot-crdt/src/board_registry.rs crates/knot-server/src/main.rs
git commit -m "feat(boards): wire BoardRoom to the Bus for cross-pod convergence"
```

---

## Task 3: Cross-pod fan-out integration test

**Files:**
- Create: `crates/knot-crdt/tests/board_fanout.rs`

Context: knot-crdt dev-deps include `knot-test-support` and `sqlx`; knot-storage is a normal dep so all `Pg*Store`s and `sort_key_between` are usable from this test. `setup()` in `tests/boards.rs` is NOT importable across crates — **replicate the fixture inline**: `let pool = knot_test_support::fresh_db().await.pool;` then `PgWorkspaceStore::new(pool.clone()).create("default","W")` → `PgUserStore::new(pool.clone()).create_local("a@x.test","A","$h$")` → `add_member(ws.id, u.id, WorkspaceRole::Owner)` → `PgDocStore::new(pool.clone()).create(ws.id, None, "Doc", &sort_key_between(None,None), u.id)` → `PgBoardStore::new(pool.clone()).create(doc.id, u.id, None)` → `board.id`. `MemBus` is in-process, so two `BoardRoom`s sharing one `Arc<MemBus>` simulate two pods. Imports come from `knot_crdt::{BoardRoom, MemBus, YrsEngine, Engine, Bus}` and `knot_crdt::board_room::{Event, InMsg, ConnHandle, ConnId}` (these are `pub`). `ConnHandle { tx: mpsc::Sender<Vec<u8>> }` — make a `tokio::sync::mpsc::channel::<Vec<u8>>(64)` and read its receiver to observe what a connection receives. See `crates/knot-crdt/tests/bus_pg.rs` for `fresh_db()` usage.

- [ ] **Step 1: Write the test**

Create `crates/knot-crdt/tests/board_fanout.rs`. Build the fixture (ws→user→doc→board) via the storage `Pg*Store`s (mirror `tests/boards.rs` setup), then:

```rust
// Pseudocode shape — fill in with the real fixture + yrs update bytes:
// 1. let db = knot_test_support::fresh_db().await;
//    let store = Arc::new(PgBoardStore::new(db.pool.clone()));
//    let board_id = /* create ws, user, doc, board via the storage stores */;
// 2. let bus = Arc::new(MemBus::new());
//    let engine: Arc<dyn Engine> = Arc::new(YrsEngine);
//    let sub_a = bus.subscribe(board_id).await.unwrap();
//    let sub_b = bus.subscribe(board_id).await.unwrap();
//    let room_a = BoardRoom::spawn(board_id, engine.clone(), store.clone(), bus.clone(), sub_a).await.unwrap();
//    let room_b = BoardRoom::spawn(board_id, engine.clone(), store.clone(), bus.clone(), sub_b).await.unwrap();
// 3. Join a connection to room B and capture its outbound channel:
//    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
//    let (rtx, rrx) = tokio::sync::oneshot::channel();
//    room_b.tx.send(Event::Join { conn_id: Uuid::new_v4(), handle: ConnHandle { tx }, reply: rtx }).await.unwrap();
//    let _initial = rrx.await.unwrap().unwrap(); // initial state frame
// 4. Produce a real yrs update on a fresh doc (set a key in the "elements" map),
//    encode it as an update, and send it to room A as Inbound:
//    room_a.tx.send(Event::Inbound(InMsg { from: Uuid::new_v4(), bytes: update })).await.unwrap();
// 5. ASSERT room B's connection receives a framed update within ~2s:
//    let got = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap();
//    assert!(got.is_some(), "room B did not receive the cross-pod update");
```

Use the `make_*` yrs update helpers from `room.rs` tests as a reference for constructing a valid update (e.g. build a `yrs::Doc`, mutate `get_map("elements")`, `encode_state_as_update_v1`). Add a second test asserting **presence** published on room A reaches room B's connection (send `Event::AwarenessIn` to room A, expect room B's channel to receive the payload).

Mark both `#[tokio::test(flavor = "multi_thread")]`.

- [ ] **Step 2: Run the test**

Run: `cargo nextest run -p knot-crdt --test board_fanout`
Expected: PASS. (If timing is tight, the `MemBus` is in-process so delivery is fast; a 2s timeout is ample. If the initial-state `reply` ordering trips you up, drain the join reply before sending the Inbound, as sketched.)

- [ ] **Step 3: Commit**

```bash
git add crates/knot-crdt/tests/board_fanout.rs
git commit -m "test(boards): cross-pod convergence via shared Bus (two rooms)"
```

---

## Task 4: Docs + full verification

**Files:**
- Modify: `README.md`, `deploy/helm/knot/values.yaml` (comment) or `deploy/helm/knot/README.md`

- [ ] **Step 1: Update the docs that said boards are single-node**

- In `README.md` Status section, the line about single-replica was slimmed already; if it implies boards block multi-pod, soften it — boards now fan out via the bus like documents. Keep it accurate and brief (e.g. drop any remaining "documents-only HA" implication).
- Wherever `deploy/helm/knot/values.yaml` comments on `replicaCount` being limited by boards, update the comment to note boards now support multi-pod. Do NOT change the default value in this task (raising the default is a separate decision); just correct any now-false comment.
- Grep for stale claims: `grep -rni "single-node\|documents-only\|cross-pod" README.md ARCHITECTURE.md deploy/helm/knot` and fix any that are now false re: boards. Note: `board_room.rs`'s header comment ("single-node for v0.1 ... bus can be added later") is now obsolete — update it to describe the implemented bus wiring.

- [ ] **Step 2: Commit docs**

```bash
git add README.md deploy/helm/knot crates/knot-crdt/src/board_room.rs
git commit -m "docs(boards): boards now scale across pods via the Bus"
```

- [ ] **Step 3: Full verification**

Run:
- `cargo nextest run -p knot-storage -p knot-crdt -p knot-server` → all pass (incl. `boards`, `board_fanout`).
- `cargo clippy -p knot-storage -p knot-crdt -p knot-server --all-targets -- -D warnings` → clean.
- `cargo build` (whole workspace) → green.

- [ ] **Step 4: Manual note**

True multi-pod behavior can't be exercised on a single dev process (one in-process `BoardRoom` per board). The `board_fanout` two-room test is the cross-pod proof. To smoke it for real later: run two server replicas behind the dev compose against one Postgres and open the same board in two browsers pointed at different replicas.

---

## Self-Review notes

- **Spec coverage:** `BoardStore::since` (Task 1) ✓; `BoardRoom` bus wiring — publish on edit/awareness, two remote arms, watermark (Task 2) ✓; registry subscribe/unsubscribe + construction reuse of the existing bus (Task 2) ✓; cross-pod two-room `MemBus` test + store test (Tasks 1,3) ✓; docs update (Task 4) ✓; no schema change ✓; no e2e (documented why) ✓.
- **Type/signature consistency:** `since(id, after_seq) -> Vec<(i64, Vec<u8>)>` used identically in Task 1, the Task 2 remote-update arm, and the Task 3 test. `BoardRoom::spawn(board_id, engine, store, bus, subscription)` and `BoardRooms::new(engine, store, bus)` consistent across Tasks 2 and 3. Watermark field `last_applied_seq` consistent.
- **Watermark correctness:** set on local apply before publish (self-NOTIFY no-op) and advanced in the remote arm; remote updates are applied but NOT re-persisted (already in the log).
