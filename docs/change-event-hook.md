# Change event hook

knot can tell another system which pages changed. It opens a WebSocket
connection to a receiver that you run and sends one JSON message per
changed page. The message names the page and the kind of change; it does
not carry content. Fetch the content through the API.

Set these environment variables on knot to turn the hook on:

| Variable | Meaning |
|----------|---------|
| `KNOT_CHANGE_EVENT_HOOK_URL` | Receiver URL, `ws://` or `wss://`. Unset or empty turns the hook off. knot refuses to start with any other scheme. |
| `KNOT_CHANGE_EVENT_HOOK_TOKEN` | Optional. Sent as a bearer token on the handshake. Printable ASCII only. |

With the Helm chart, use `changeEventHook.url` and
`changeEventHook.tokenSecretName` (see `deploy/helm/knot/README.md`).

## Handshake

knot connects to the URL with an ordinary WebSocket upgrade request. If a
token is configured, the request carries `Authorization: Bearer <token>`.
To refuse a connection, answer the upgrade with 401 or 403.

A failed connection attempt (refused, rejected, or no answer within 10
seconds) is retried with exponential backoff: the delay starts at 0.5
seconds, doubles up to 30 seconds, and is jittered. A successful connection
resets the delay. `wss://` receivers need a certificate that chains to a
public root (the Mozilla root set); knot does not read the system trust
store.

## Messages

Every message is one JSON text frame. knot sends a `hello` frame first,
then `page_changed` frames.

```json
{"type":"hello","instance_id":"3f0c1a52-8d4e-4f6b-9a77-2b1c5e0d9a10","version":"0.10.0"}
```

| Field | Meaning |
|-------|---------|
| `instance_id` | Random UUID of the knot process. It changes when the process restarts and stays the same across reconnects. |
| `version` | knot version. |

```json
{"type":"page_changed","doc_id":"b6d1c4a0-...","workspace_id":"7e2a9f10-...","kind":"content","at":"2026-11-22T10:00:01.234Z"}
```

| Field | Meaning |
|-------|---------|
| `doc_id` | UUID of the page. |
| `workspace_id` | UUID of the workspace the page belongs to. Events cover every workspace. |
| `kind` | What happened; see below. |
| `at` | Time the batch was sent (RFC 3339, UTC). All events sent from one window share it. |

`kind` is one of:

| Kind | Sent when |
|------|-----------|
| `content` | The page body changed: an edit in the editor, a markdown write through the API, a markdown import, a history restore. |
| `created` | A new page exists: created directly, duplicated (one event per page of the copy), made from a template, created by moving content to a subpage, copied from a history version, or imported. |
| `renamed` | The title or icon changed. |
| `moved` | The page was moved to another parent or reordered. |
| `archived` | The page was archived. Archiving a page archives its subpages, and each page gets its own event. |
| `restored` | The page was restored from the archive, one event per page of the subtree. |
| `purged` | The page was permanently deleted, by hand or by the retention job. One event per page of the subtree. |

Ignore messages whose `type` you do not know, and events whose `kind` you
do not know. knot may add both later. knot does not read what you send
back, except WebSocket pong and close frames.

## Delivery guarantees

Delivery is at most once. Treat every event as a hint that a page may have
changed, not as a log of what happened.

- knot coalesces changes per page and kind. A change opens a window of about
  one second; further changes of the same page and kind inside it add
  nothing. A page edited a hundred times in a second yields one `content`
  event. A page created and edited in the same window yields two events,
  `created` and `content`, with the same `at`.
- knot does not replay events. An event is lost if the connection is down
  when its window closes, if the connection drops while it is being sent, if
  knot restarts first, or if knot's internal queue (4096 entries) is full.
- Events from different connections have no defined order.
- Restricted pages are included. The hook sees every change in every
  workspace, so a `content` event can name a page that your API key cannot
  read.

So read the current state when an event arrives: `GET /api/docs/{id}` for
metadata and `GET /api/docs/{id}/markdown` for the body, with a scoped API
key. A `404` after a `purged` or `archived` event is expected. If you need a
complete picture, reconcile through the API, for example with a full
listing, after a connection opens and whenever you see a new `instance_id`.
A periodic reconcile also covers events lost to a full queue.

## Many connections

Each knot pod opens its own connection. The number of connections you
receive therefore follows the number of pods: it grows and shrinks with
scaling, and during a rolling update it reaches the replica count plus the
surge. Plan for that many concurrent connections.

- Every change arrives on exactly one connection, the one from the pod
  where the change happened. A receiver that sees one connection sees only
  part of the changes.
- There is no ordering across connections.
- Tell pods apart by `instance_id`. It changes on every process restart and
  stays the same when a connection reconnects.
- Connections close during deploys, restarts and scale-down. A single
  disconnect is normal. Alert on a missing receiver or on a sustained
  `knot_change_event_hook_connected` of 0, not on one closed connection.
- You can spread connections across receiver instances behind a load
  balancer. Keep no state that ties a page to one connection.

knot does no coordination between pods. The receiver owns this: accept many
connections, merge their events, and make handling idempotent.

## Keepalive

knot sends a WebSocket ping every 30 seconds. It closes the connection
after two pings go unanswered and then reconnects. WebSocket libraries
answer pings automatically while your code reads from the connection. A
receiver that stops reading stops answering.

## Operating

Metrics (on knot's metrics endpoint, if enabled):

| Metric | Type and labels | Meaning |
|--------|-----------------|---------|
| `knot_change_event_hook_connected` | gauge | 1 while this pod is connected to the receiver, else 0. |
| `knot_change_event_hook_events_sent_total` | counter | Events written to the connection. |
| `knot_change_event_hook_events_dropped_total` | counter, `reason` = `queue_full`, `disconnected`, `send_failed` | Events dropped before they were sent. |
| `knot_change_event_hook_connects_total` | counter, `result` = `ok`, `error` | Connection attempts. |

Log lines, from the `knot-server` process:

| Level | Message | Meaning |
|-------|---------|---------|
| info | `change event hook: connected` | A connection is open and `hello` is sent. |
| warn | `change event hook: connect failed` | The attempt failed; the line carries the error. A timeout logs `connect timed out`. |
| warn | `change event hook: connection lost, reconnecting` | An open connection ended. |
| warn | `change event hook: receiver stopped answering pings` | Two pings went unanswered. |

## Minimal receiver

This receiver uses the Python
[`websockets`](https://websockets.readthedocs.io/) package (version 14 or
later, `websockets.asyncio.server` API). It checks the token and prints every
message.

```python
import asyncio
import json
import os

from websockets.asyncio.server import serve

TOKEN = os.environ.get("HOOK_TOKEN")


def check_token(connection, request):
    if TOKEN and request.headers.get("Authorization") != f"Bearer {TOKEN}":
        return connection.respond(401, "unauthorized\n")


async def handle(ws):
    async for text in ws:
        msg = json.loads(text)
        if msg.get("type") == "hello":
            print("pod", msg["instance_id"], "version", msg["version"])
        elif msg.get("type") == "page_changed":
            print(msg["kind"], msg["doc_id"], "at", msg["at"])


async def main():
    async with serve(handle, "0.0.0.0", 9000, process_request=check_token):
        await asyncio.get_running_loop().create_future()


asyncio.run(main())
```

Point knot at it with `KNOT_CHANGE_EVENT_HOOK_URL=ws://receiver:9000`. The
example compares the token with `!=` for brevity; use
`hmac.compare_digest` in production, and `wss://` outside a trusted network.
