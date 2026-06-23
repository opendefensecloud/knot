# Horizontally scalable Excalidraw boards — design

**Date:** 2026-06-23
**Status:** Approved (brainstorm)

## Problem

Documents are horizontally scalable: the `Room` actor is wired to the `Bus`
(`crates/knot-crdt/src/bus.rs` → `PgBus`, Postgres `LISTEN/NOTIFY`), so any pod can
serve any document and edits converge across replicas. Excalidraw **boards** are
not: `BoardRoom` (`board_room.rs`) applies updates, persists them, and broadcasts
**only to connections on the same pod**. Its header says so: *"no bus integration
(single-node for v0.1 — multi-node bus can be added later by wiring the existing
`Bus` trait into this loop)."* With two replicas, users on different pods editing the
same board never see each other. This is the sole reason the Helm chart defaults to
`replicaCount: 1`.

## Goal

Boards converge across pods exactly like documents, by wiring the existing `Bus`
into `BoardRoom`. No new infrastructure, no schema change.

## Why this is small

The hard prerequisite already exists. `BoardStore` is a durable, seq-ordered update
log identical in shape to the document `UpdatesStore`:
- `append_update(id, bytes) -> Result<i64>` already **returns the new seq**;
- `max_update_seq(id) -> Result<i64>` exists;
- `board_updates` already has the `seq` column.

So the board log already supports "fetch updates since seq" semantics — only the
query method is missing. Everything else is wiring that mirrors the document `Room`,
which is the reference implementation.

## Design

### Decision: reuse the existing `Bus` (no PgBus change)

`PgBus` uses **per-UUID channels** — `LISTEN "doc:{uuid}"` / `"presence:{uuid}"`
(`bus_pg.rs:208-211`), routed by channel-name prefix. Board IDs are distinct random
UUIDs from doc IDs, so a board publishing on `doc:{board_id}` never collides with any
document: a pod only listens on `doc:{board_id}` when it has that board open. We pass
the **same `Arc<dyn Bus>`** the document `Rooms` already use into `BoardRooms`. Zero
new infra, one `LISTEN` connection per replica, no change to the document path. The
only cost is cosmetic (a board's internal channel name is prefixed `doc:`); a code
comment notes it.

### 1. `BoardStore::since` (the one store addition)

Add a trait method + Postgres impl mirroring `UpdatesStore::since`:

```rust
/// Board updates with seq > after_seq, in seq order. Used for incremental
/// cross-pod catch-up (the bus delivers the seq; the room fetches the bytes).
async fn since(&self, id: Uuid, after_seq: i64) -> Result<Vec<BoardUpdate>>;
```

where `BoardUpdate { seq: i64, bytes: Vec<u8> }` (or `(i64, Vec<u8>)`). `board_updates`
already has `seq`; **no migration**. `load_updates` (full tail, for boot) is unchanged.

### 2. `BoardRoom` — wire the bus (mirror `Room`)

- New fields: `bus: Arc<dyn Bus>`, `bus_updates_rx: mpsc::Receiver<i64>`,
  `bus_presence_rx: mpsc::Receiver<Vec<u8>>`, `last_applied_seq: i64`.
- `spawn(board_id, engine, store, bus, subscription)`: after hydration set
  `last_applied_seq = store.max_update_seq(board_id)`; store the subscription's
  `updates`/`presence` receivers.
- Two new `select!` arms in `run` (exactly like `Room` at `room.rs:452-455`):
  - **remote update** `Some(_seq) = bus_updates_rx.recv()`: `store.since(board_id,
    last_applied_seq)` → for each (apply to local doc, `wrap_sync_update`, broadcast
    to local conns) → advance `last_applied_seq`. Skip entries `<= last_applied_seq`.
  - **remote presence** `Some(payload) = bus_presence_rx.recv()`: broadcast `payload`
    to local conns.
- `on_inbound` (local edit): after `append_update` returns `seq`, **set
  `last_applied_seq = seq`** then `bus.publish(board_id, seq)`. Keep the existing
  local apply + persist + local fan-out. Setting the watermark first makes the
  self-NOTIFY a no-op (`since(seq)` returns nothing), so no double-apply.
- `AwarenessIn` (local presence): keep the local fan-out (minus origin), then add
  `bus.publish_presence(board_id, payload)`. Self-echo over the bus is harmless —
  awareness is idempotent last-writer-wins state (this matches the document path,
  which does not special-case self-presence).

### 3. `BoardRooms` registry — subscribe/unsubscribe

- `BoardRooms::new(engine, store, bus)` (add the `bus` param).
- `acquire`: `let sub = bus.subscribe(board_id).await?` then
  `BoardRoom::spawn(board_id, engine, store, bus, sub)` (mirrors `Rooms::acquire` at
  `registry.rs:82-83`).
- `evict`: `bus.unsubscribe(board_id).await` after cancelling the room (mirrors
  `Rooms` doc eviction at `registry.rs:126`) so subscriptions don't leak.

### 4. Construction

Thread the existing bus `Arc` into `BoardRooms::new` where it is constructed
(alongside the document `Rooms`); `AppState` already holds both `bus` and
`board_rooms`. In-memory/test construction passes a `MemBus`.

## Components / files

- `crates/knot-storage/src/boards.rs` — `BoardUpdate` type (if not present) +
  `BoardStore::since` + `PgBoardStore` impl.
- `crates/knot-crdt/src/board_room.rs` — bus fields, two `select!` arms, publish on
  inbound/awareness, watermark.
- `crates/knot-crdt/src/board_registry.rs` — `new(.., bus)`, subscribe in `acquire`,
  unsubscribe in `evict`.
- The crate that constructs `BoardRooms` (server `lib.rs` / wherever `BoardRooms::new`
  is called) — pass the bus.

## Testing

- **Store** (`crates/knot-storage/tests/boards.rs`): `since(id, after)` returns only
  updates with `seq > after`, in order; `since(id, 0)` returns all; `since(id, max)`
  returns none.
- **Cross-pod fan-out** (`crates/knot-crdt`): the load-bearing test. Spawn **two**
  `BoardRoom`s for the same `board_id` sharing a single `MemBus` and one
  `PgBoardStore` (`fresh_db`) — this simulates two pods. Join a connection to room B,
  send an `Inbound` y-update to room A, and assert room B's connection receives the
  framed update (cross-pod convergence). A second case: presence published on room A
  reaches room B's connection. Mirror the existing `MemBus` room tests
  (`room.rs:777+`).
- **No e2e:** a single dev process runs one `BoardRoom` per board, so both browser
  clients already converge in-process *without* the bus — an e2e cannot exercise the
  cross-pod path and would pass even today. The two-room `MemBus` test is the real
  proof. (Documented here so the gap is intentional, not an oversight.)
- **Regression:** existing board + room tests stay green; `clippy -D warnings`.

## Risks / notes

- **Self-NOTIFY:** Postgres delivers a pod's own `NOTIFY` back to it. The
  `last_applied_seq` watermark (set on local apply before publish) makes the
  self-update a no-op; presence self-echo is idempotent. Same model as documents.
- **Boot race:** between hydration (`max_update_seq`) and the first `bus_updates_rx`
  poll, a concurrent remote write bumps the seq; the next NOTIFY triggers
  `since(last_applied)` which still returns it — no loss. (The subscription is created
  in `acquire` before `spawn`, so notifications during hydration queue in the channel.)
- **Out of scope:** board snapshotting (boards replay the full update tail on boot;
  fine while boards are small — independent improvement); raising the Helm
  `replicaCount` default (a follow-up once this lands and is validated).
- After this lands, the README/Helm note that boards are single-node becomes
  outdated — update them as part of the change.
