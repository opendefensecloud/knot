# Change event hook — design

Status: agreed 2026-11-22. Plan: `docs/superpowers/plans/2026-11-22-change-event-hook.md`.

## Goal

Tell an external receiver, as it happens, which pages changed, so it can mirror, index or react to them without polling. Knot connects out to a receiver the operator configures and keeps that connection open; the receiver gets a stream of page UUIDs.

## Decisions

| # | Topic | Decision |
|---|---|---|
| D1 | Transport | WebSocket. Knot is the **client**; the receiver is a WebSocket server at `ws://` or `wss://`. One text frame per message. |
| D2 | Configuration | Env only, like every other setting: `KNOT_CHANGE_EVENT_HOOK_URL` (config field `change_event_hook_url`, prose name `change-event-hook-url`). Empty (default) = feature off, no task spawned. Optional `KNOT_CHANGE_EVENT_HOOK_TOKEN` (`change_event_hook_token`). |
| D3 | What is a change | Content edits (CRDT updates applied on this pod: editor, markdown import, history restore) and page lifecycle: create (incl. duplicate, template, import, move-to-subpage), rename/icon, move, archive, restore, purge (manual and retention). **Not** grants or the restricted flag. |
| D4 | Once per change | Changes are coalesced per `(doc_id, kind)` over a window (production 1 s). The window opens at the first pending change and closes `window` later; at close, one event per `(doc_id, kind)` is sent. A page is never reported twice for the same kind in one window; every window with a change yields exactly one event per kind. |
| D5 | Many pods | Every pod keeps its own connection and reports only changes that **originated on that pod**. Content events come from a new local-only room hook (fired after the local writer persists), never from updates received over the bus. Managing many concurrent connections is the receiver's job. |
| D6 | Wire format | JSON. First frame after each connect: `{"type":"hello","instance_id":"<uuid, random per process>","version":"<crate version>"}`. Then `{"type":"page_changed","doc_id":"<uuid>","workspace_id":"<uuid>","kind":"content|created|renamed|moved|archived|restored|purged","at":"<RFC 3339 UTC>"}`. `at` is the time the window closed; every event of one window carries the same `at`. No titles, no content. |
| D7 | Authentication | If a token is set, the handshake carries `Authorization: Bearer <token>`. `wss://` uses rustls with webpki roots. |
| D8 | Reconnect / keepalive | Exponential backoff 0.5 s → 30 s, each delay drawn from `[cur/2, cur]` (jitter that never collapses to zero), reset after a successful handshake. Handshake timeout 10 s; a send that does not complete in 10 s = dead connection. Ping every 30 s; 2 consecutive pings without a pong = dead connection. Events go through a bounded queue (4096); dropped when full or when no connection is up. **At most once**: nothing is buffered across a reconnect, nothing is retried. Metrics: `knot_change_event_hook_connected` (gauge 0/1), `knot_change_event_hook_events_sent_total`, `knot_change_event_hook_events_dropped_total{reason="queue_full"|"disconnected"|"send_failed"}`, `knot_change_event_hook_connects_total{result="ok"|"error"}`. |
| D9 | Incoming frames | Only Pong and Close are acted on; everything else is ignored (logged at debug). |
| D10 | Bad config | A non-empty URL that does not parse or is not `ws`/`wss` fails `Config::load` (`ConfigError::Invalid`, exit 2). A valid but unreachable URL logs a warning and retries. |
| D11 | Restricted pages | Included. Events cover every page of every workspace, whatever its restriction. The receiver learns only UUIDs; details need an API key with access. |
| D12 | Bulk operations | One event per affected page (archive/restore/purge subtree, duplicate with subpages, workspace import). A burst may overflow the queue; that is within the at-most-once contract. |
| D13 | Test knobs | A builder in `knot-server`: `change_events::Hook::new(url).token(..).backoff(min, max).window(d).ping_interval(d).queue_capacity(n)`. Production defaults; `main.rs` sets only URL and token. No extra env vars. |
| D14 | Receiver docs | `docs/change-event-hook.md`, linked from `README.md` and `deploy/helm/knot/README.md`. Covers protocol, schema, guarantees, multi-connection duties. |

## Architecture

```
HTTP handlers (create/rename/move/archive/restore/purge/duplicate/import/...)
   └─ state.change_events.emit(workspace_id, doc_id, kind) ─┐   try_send, never blocks
Room actor (knot-crdt) on_applied (local persist only)      │
   └─ local_change_tx.try_send(doc_id) ─────────────────────┤
                                                            ▼
                         change_events coalescer task (window, dedupe by (doc_id, kind),
                         resolves workspace_id for content events via DocStore::get)
                                                            │ batch, dropped if not connected
                                                            ▼
                         change_events connection task (connect, hello, send, ping/pong,
                         backoff + reconnect, shutdown on CancellationToken)
                                                            │ WebSocket
                                                            ▼
                                                        receiver
```

- `knot-crdt`: `Rooms::with_local_change_tx(mpsc::Sender<Uuid>)`, threaded to each `Room` like `dirty_tx`, fired only in `Room::on_applied`.
- `knot-server`: new module `change_events` (`mod.rs` public API, `coalesce.rs`, `conn.rs`). `AppState.change_events: Option<ChangeEvents>`; `None` when off, so every emit site is a no-op.
- `knot-config`: two string fields plus validation.

## Out of scope

Grant/restriction changes, comments, boards (Excalidraw) content, retries or durable delivery, a receiver implementation, per-workspace filtering.

## Testing

`crates/knot-server/tests/change_event_hook_integration.rs`, in-process (fresh_db, AppState, Rooms, axum on 127.0.0.1:0, receiver on its own TcpListener):

1. **Reconnect** — background writer creates 5 pages and edits them round-robin every ~50 ms via `POST /api/docs/{id}/markdown`; three cycles of accept → hello → ≥1 valid UUID → no duplicate `(doc_id, kind)` per `at` → disconnect (abrupt TCP drop, close frame, abrupt drop); same `instance_id` across cycles; 10 s per cycle, 30 s total; backoff 50–200 ms, window 100 ms.
2. **Once per change** — window 2 s; 20 edits to one page in under one window → exactly one `content` event, then nothing in the next quiet window.
3. **Lifecycle kinds** — table-driven: create, rename, move, archive, restore, purge, each yielding its `kind` for the right `doc_id`.
4. **Two pods** — two AppStates on one database, each with its own PgBus, Rooms and hook; page open on both; an edit via A yields an event on A's connection only.
5. **Off** — unit test: no URL → `Hook::from_config` returns `None`; config tests for default and env loading and rejection of bad URLs.
