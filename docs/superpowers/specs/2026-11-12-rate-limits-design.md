# Rate limits and quotas for writes — design

**Date:** 2026-11-12
**Status:** Reviewed (security, normal use, WebSocket correctness, operator
surface), revised
**Issue:** #88

## Context

Apart from the login throttle, knot doesn't limit how often or how much a
signed-in user can write. A compromised, scripted or misused account can
create pages, upload files, duplicate subtrees or import archives in a loop
until Postgres or blob storage is full. Restricted pages make such a flood
hard to see: an owner can't see pages inside a restriction they have no
grant for.

Today every limit bounds **one request**:

- 2 MiB default body and a 30 s timeout (`crates/knot-server/src/lib.rs`)
- 10 MiB per blob, 1 MiB per board SVG, 4 MiB per WebSocket message
- 200 pages / 200 MiB per duplicate; two duplicates and two DOCX/ODT
  exports at a time per process (503 + `Retry-After`)
- 50 MiB per workspace import — of **compressed** body only. The manifest
  can list any number of pages, and any number of attachment entries can
  point at the same zip entry, so one import can write gigabytes
  (`routes/api/export_import.rs`, the attachment loop re-reads the entry
  for every reference).

Nothing limits **how many** requests one user or client sends, nor how much
a workspace stores in total. The collab WebSocket has no per-connection or
per-user rate at all, and nothing caps how many sockets a user holds.

The one existing limiter, the login throttle
(`crates/knot-auth/src/throttle.rs`), keys on the TCP peer's IP. knot has no
proxy handling, so behind the Helm ingress every client has the ingress
pod's address: five failed logins anywhere block password sign-in for
everyone for minutes. Its keys are never pruned.

## Goals

1. A per-user rate limit on mutating API routes, with separate, tighter
   buckets for expensive operations.
2. Per-client-IP limits on anonymous routes that are **never worse than
   today** when the proxy setup is unknown.
3. Rate limits on the collab WebSocket that **never lose or refuse an
   edit**, that normal typing, pasting and drawing don't reach, and that
   bound what one user can make a room do.
4. Always-on caps on what one import may write.
5. Optional quotas, off by default: pages and file bytes, per user and per
   workspace.
6. Operator control through config and Helm values, a counter for rejected
   requests, a Grafana panel and an alert.
7. A correct client IP behind a reverse proxy, when the operator says which
   proxies to trust (fixes the login throttle's shared bucket).
8. A clear message in the app when a limit or quota is hit.

## Non-goals

- Limiting reads (search, page loads, history previews), except exports.
- A shared (cross-replica) rate-limit store.
- Per-document size caps on the CRDT; quotas on CRDT history bytes or board
  SVGs. The WS byte budget is a CPU and bus guard, not a storage guard.
- Notifying owners; a UI for usage.
- An `Origin` check on the collab upgrade (a separate hardening issue).
- Billing-grade accuracy: these are abuse guards.

## Anti-goals (would make this a failure even if it "works")

- A limit that drops or refuses a collab edit, or makes a user reload.
- Normal use tripping a limit: a 40-page tree seeded in a second, a
  multi-file drop, an hour on a board, skimming the tree fast, a morning
  sign-in rush, a reconnect storm after a rollout or a role change.
- A new way for anyone to lock everyone out of sign-in.
- A client that retries a 429 in a tight loop, or a new WS close code that
  old bundles answer with a reconnect loop.
- Limiter memory an attacker can grow without bound.
- A spoofed `X-Forwarded-For` letting a client choose its bucket when the
  operator did not opt in to trusting proxies.
- Limiting health probes or static files.
- A Helm default that crashes the pod at startup.

## Decided

### Answers to the issue's open questions

- **Per user, per workspace, or both?** Rate limits are **per user**
  (anonymous routes per client IP). knot runs one workspace per instance,
  so a per-workspace rate would be one bucket for everyone. Quotas exist
  at **both** levels.
- **Are owners exempt?** **No.** A compromised owner account is the worst
  case, the defaults are far above normal use, and the operator, not the
  owner, sets them.
- **Audit log or notify owners?** A quota refusal is audited
  (`quota.exceeded`), **at most once per user, quota and scope per
  10 minutes**, so a script at its quota can't turn refusals into audit
  rows. A 429 is never audited; it goes to the metrics counter and to a log
  line when a key *starts* being limited. Owner notifications are deferred.
- **Collab edits, and fast typing?** Per user, by bytes and messages,
  enforced by **backpressure** (the server waits before reading the next
  frame; nothing is dropped). Doc and board sockets have separate budgets,
  so a big board drag never slows typing. Presence is coalesced, never
  lost. Defaults come with headroom over measured-by-reasoning peaks and
  are a CPU/bus guard.
- **Multiple replicas?** Buckets live **in each replica's memory**. A user
  spread over N replicas gets up to N × the limit; `rateLimit.scale`
  lets the operator divide. A shared store would cost a Postgres round
  trip per write and per WS frame. **Quotas are counted in Postgres** and
  hold across replicas.

### Configuration (small on purpose)

| Setting | Env | Default | Meaning |
|---|---|---|---|
| `rate_limit_enabled` | `KNOT_RATE_LIMIT_ENABLED` | `true` | every HTTP and WS rate limit, not quotas or import caps |
| `rate_limit_scale` | `KNOT_RATE_LIMIT_SCALE` | `1.0` | multiplies every burst and rate below; in (0, 100] |
| `rate_limit_collab_max_sockets` | `KNOT_RATE_LIMIT_COLLAB_MAX_SOCKETS` | `64` | open collab sockets per user per replica; ≥ 1 |
| `trusted_proxies` | `KNOT_TRUSTED_PROXIES` | empty | comma-separated IPs/CIDRs; `0.0.0.0/0`, `::/0` (and `::ffff:0:0/96`) refused; IPv4-mapped CIDRs (`::ffff:10.0.0.0/104`) become IPv4 (`10.0.0.0/8`), mapped CIDRs below /96 are refused; host bits after the prefix (`10.0.0.5/8`) are refused, not widened |
| `quota_pages_per_user` | `KNOT_QUOTA_PAGES_PER_USER` | `0` (off) | |
| `quota_pages_per_workspace` | `KNOT_QUOTA_PAGES_PER_WORKSPACE` | `0` | |
| `quota_blob_bytes_per_user` | `KNOT_QUOTA_BLOB_BYTES_PER_USER` | `0` | |
| `quota_blob_bytes_per_workspace` | `KNOT_QUOTA_BLOB_BYTES_PER_WORKSPACE` | `0` | |

Per-bucket numbers are constants (below); per-bucket overrides can come
later if someone needs them. Scaled values never go below a floor (1 token;
WS bytes 64 KiB/s and a burst of at least one max-size frame, 4 MiB; WS
messages 20/s), so no setting can make a wait last days.

`AppState::in_memory()` / `with_pool()` set `rate_limit_enabled = false` in
their config, so the integration tests keep running unlimited; `main`
already replaces `config` with the operator's. The limiter is built from
`state.config` in `router_with_state`, so there is no separate wiring that
`main` could forget. Tests that want limits set the config first.

### HTTP buckets

A token bucket per (bucket, key): `burst` tokens, refilled at
`per_minute / 60` per second (× scale). Each limited request takes one
token from exactly **one** bucket, chosen from its method (HEAD counts as
GET) and route template (`MatchedPath`):

| Bucket | Key | Routes | Burst, per minute |
|---|---|---|---|
| `expensive` | user | `POST /api/docs/{id}/duplicate`, `POST /api/workspace/import`, `POST /auth/password` | 10, 10 |
| `export` | user | `GET /api/workspace/export`, `GET /api/docs/{doc_id}/export`, `POST /api/docs/{id}/export/{format}` | 20, 20 |
| `upload` | user | `POST /api/docs/{doc_id}/blobs` | 60, 60 |
| `board_preview` | user | `PUT /api/boards/{id}/svg` (sent 300 ms after every board change; overwrites one row) | 300, 300 |
| `write` | user | every other `POST`/`PUT`/`PATCH`/`DELETE` under `/api` | 120, 120 |
| `sign_in` | client IP | `POST /auth/login`, `POST /auth/setup` | 30, 30 |
| `public` | client IP | `GET /p/{token}`, `GET /p/{token}/blobs/{id}`, `GET /p/{token}/boards/{id}/svg` | 600, 600 |

Not limited: other `GET`s, health, version, static files and the SPA
fallback, `/auth/session`, `/auth/config`, `/auth/logout`, the OIDC routes
(top-level navigations where a 429 would show raw JSON; `/auth/oidc/login`
only sets a cookie and redirects, the callback is bound to its state cookie
and the IdP), `GET /api/short-links/{code}` (needs a session; same 404 for
unknown and unreadable).

- Mutating routes under `/api` default to `write`, so a new route is
  limited without anyone listing it. The explicit lists are pinned by a
  test that each template names a real route.
- The `/api` buckets run **after** the session and CSRF checks (a 401 or a
  CSRF failure costs nothing) and **before** the doc-role check and the
  handler (a refused request costs no ACL lookup). `/auth/*` and `/p/*` sit
  outside the api router and get their own layer; `/auth/password` is
  charged in its handler after its session check and after a check that
  the body is `Content-Type: application/json` (400 `auth.bad_request`
  otherwise): `/auth/*` has no CSRF layer, and a plain form from a
  same-site page can't send that type without a CORS preflight, so such
  forms can't drain the `expensive` bucket that duplicate and import
  share.
- Exports have their own bucket because they are GETs: a cross-site
  navigation carries the Lax cookie, so a hostile page can drain `export`
  but not `expensive` or `write` (a same-site one neither: `write` needs
  the CSRF header, `/auth/password` a JSON content type).
- **IP buckets are never worse than today.** When `trusted_proxies` is
  empty and a request carries `X-Forwarded-For`, knot is behind a proxy it
  wasn't told about, every client looks alike, and a bucket would be shared
  by everyone: the IP buckets are **skipped** for that request, a warning is
  logged once and `knot_client_ip_unresolved_total{reason="no_trusted_proxies"}`
  counts it. With proxies trusted, an untrusted peer that sends the header
  is limited as itself and counted under `reason="untrusted_peer"` (a direct
  client can do that, so it is no sure sign of a missing proxy; its warning
  is logged once too). These counters count only while rate limits are on.
  Without the header, or with the proxy trusted, they apply. (A direct
  client that adds the header itself only gets back today's behaviour.)

### The 429 answer

```
HTTP/1.1 429 Too Many Requests
Retry-After: 7
{"error":{"code":"rate.limited","message":"too many requests","details":{"retry_after_secs":7}}}
```

`retry_after_secs` is the whole seconds until one token is back, at least 1.
The client reads it from `details` (both `apiFetch` and the raw-fetch paths
already copy `details`); the header is for other clients. The bucket name
is in the metric, not the body.

One exception: a refused `GET`/`HEAD /p/{token}` whose `Accept` names
`text/html` (a browser opening a share link, which the server answers
itself with whole HTML pages, its 410 and 503 included) gets a small HTML
page, "Too many requests … Try again in N s.", with the same
`Retry-After`. The page's files (`/p/{token}/blobs/…`, board SVGs) and the
SPA's own fetch of `/p/{token}` keep the JSON envelope.

### Client IP and trusted proxies

- Empty `trusted_proxies`: the client is the TCP peer (today).
- Set, and the peer is trusted: join every `X-Forwarded-For` line in order,
  split on commas, trim; accept `ip`, `ip:port`, `[v6]` and `[v6]:port`;
  canonicalise IPv4-mapped IPv6. Walk from the right, skipping trusted
  addresses; the first untrusted one is the client. If every entry is
  trusted, the **right-most entry** is the client: the address the nearest
  proxy saw. (Anything further left may come from a client inside the
  trusted range and can't be believed.) A malformed entry stops the walk;
  the last valid address read, or the peer if there is none, is the
  client, and the malformed entry is counted.
- Peer not trusted: the peer, whatever the header says.
- Keys: IPv4 per address; IPv6 per `/64`. Trust checks use the full
  address.
- Used by the IP buckets, the login and password failure throttles and the
  IP stored on a local session.
- The chart README says to list only the ingress controller's addresses,
  and to restrict `networkPolicy.ingressFrom` to it when trusting a pod
  CIDR.

### Collab WebSocket

In the per-connection loops (`crates/knot-server/src/room.rs`,
`board_room_shim.rs`, the latter now given the user id), never in the
room actor.

| Budget | Key | Burst | Refill | Over budget |
|---|---|---|---|---|
| bytes, every inbound frame (any type, any role) | user × {doc, board} | doc 32 MiB, board 64 MiB | doc 1 MiB/s, board 4 MiB/s | wait before the next read |
| messages, every non-awareness frame | user × {doc, board} | doc 1 000, board 2 000 | doc 300/s, board 600/s | wait before the next read |
| awareness frames | user × {doc, board} | 240 | 120/s | coalesced: one frame waits per socket and is sent when a token is back; a newer one is merged into it, keeping per awareness client the entry with the higher clock (a frame that doesn't decode replaces it) |
| client `SyncStep1` | connection | the first one | — | later ones are ignored (no reply) |
| open sockets | user (per replica) | `collab_max_sockets` (64) | — | upgrade refused, 429 |
| upgrades | user × target doc/board | 20 | 20/min | upgrade refused, 429 |

The read loop, written out:

```
loop {
  // before EVERY read, the first included
  while deficit = max(bytes.deficit(now), msgs.deficit(now)) > 0 {
    select! { biased;
      shutdown | revoked | refused | room_ended | done => end the session,
      sleep(min(deficit, 1 s)) => {} }      // re-check: other sockets add debt
  }
  select! { biased; shutdown | done | revoked => break,
            pending awareness ready => forward it,
            frame = stream.next() => {
              charge bytes += frame.len()  (before decode; tokens may go negative)
              if not awareness: charge msgs += 1
              handle the frame as today (awareness through the coalescer;
              the second and later SyncStep1 ignored) } }
}
```

- **Why waiting is safe:** no frame is dropped, so no update goes missing
  and no later one parks on a missing one. A frame larger than what's left
  still passes (debt) and the next read waits it off, so a 4 MiB paste or
  an offline step-2 always works. The writer is its own task, so the client
  keeps receiving while the server waits. A revoke, refusal, room restart
  or shutdown during a wait ends the session at once. When a wait (or a
  read) ends because the client is gone (the writer's send failed:
  `done` without any other signal), the loop first forwards the edits
  among the frames already readable (`SyncStep2`/`Update`; charged, no
  wait): the client may have shown them as saved, and the waiting left
  them unread in the socket's receive buffer.
- **What the user sees:** frames first fill the kernel and proxy buffers
  (several MiB) before `bufferedAmount` grows, so "Saving…" shows only once
  those are full. With the defaults, that takes a sustained paste flood;
  typing (≤ 20 KB/s) and board drags (estimated up to ~5 MB/s for 100
  elements at 120 Hz; against the 4 MiB/s refill a burst of 64 MiB lasts
  over a minute, ~80 s, and then drags slow, not fail) don't get there.
  A read waits only while the user's budget is in debt, so one socket
  owes at most the one frame it read last (≤ 4 MiB): about 4 s for a doc
  and 1 s for a board at scale 1, up to 64 s at the 64 KiB/s floor;
  longer waits need several of one user's sockets flooding at once. If the socket drops during a wait (proxy
  timeout), the client reconnects and the server's `SyncStep1` makes it
  resend what the room lacks; nothing is lost while the tab is open. The
  README says to keep proxy timeouts above a minute.
- **Presence:** coalescing keeps the latest state per awareness client on
  each socket, so the final cursor position always arrives, at most
  1/120 s late on a full budget, even when a frame about other clients
  follows it (providers also relay the removals their awareness times out
  for others). A per-user budget means more sockets don't mean more
  presence frames.
- **Vanished clients:** with limits on, the writer pings every 30 s and
  the read loop ends a session it has read nothing from (no frame, no
  pong) for 75 s; time spent in a budget wait doesn't count. So a
  half-open socket (a laptop that slept, a network change) frees its
  place under `collab_max_sockets` within about 75 s, behind a proxy too
  (pings cross it). A send the writer is stuck in (a vanished peer never
  ACKs, so a busy room fills the send buffer) is abandoned as soon as the
  read loop has ended, rather than held until TCP gives up minutes later.
  Browsers answer pings without the page's code. With limits off nothing
  is pinged, as before.
- **`SyncStep1`:** the server sends the full state on join, before reading
  anything, and an honest client sends one `SyncStep1` per connection
  (verified in both providers). Later ones only cost full-state encodes in
  the room actor, so they are ignored.
- **Upgrades:** keyed per target, so skimming the tree never trips it; it
  catches a socket that reconnects to the same doc in a loop. A refused
  upgrade is close 1006 without an open in the browser, and both providers
  back off to 30 s. No new close code.

### Import caps (always on)

- At most **10 000 pages**, **256 MiB** of page Markdown and **512 MiB**
  of attachment and board bytes per import, measured by the zip entries'
  declared uncompressed sizes summed **per reference** (an entry named
  twice counts twice: each page parses and stores its Markdown again), and
  at most **20 000 attachment and board references** together (boards
  without an SVG and references to missing entries count too), checked
  before anything is written; over any → `413 import.too_large` with
  details `{pages, bytes, markdown_bytes, references, max_pages,
  max_bytes, max_markdown_bytes, max_references}`.
- Entries are looked up by exact name (no scan of every entry's decoded
  name on a miss), so planning costs O(1) per reference.
- Each entry is read with a limit of its declared size + 1; an entry
  larger than declared is skipped (and reported like other skipped
  entries), so bytes written never exceed bytes checked. Each skip is
  logged at debug; one warning per import gives the skipped count.

### Quotas

| Setting | Counts |
|---|---|
| pages per user | pages the user created (`documents.created_by`) in the workspace |
| pages per workspace | pages in the workspace |
| file bytes per user | bytes of files the user uploaded (`blobs.created_by`) |
| file bytes per workspace | bytes of files in the workspace |

- Pages and files count **until purged**: archived ones still use storage;
  restoring never goes over a quota. Templates count.
- Duplicated pages and copied files count for the duplicating user (they
  are `created_by` the actor). Board SVGs don't count (overwritten, 1 MiB).
- Checked in the handler before anything is written:
  - `POST /api/docs`, from-template: +1 page.
  - Duplicate: +1 page in the pre-check (before the permit wait), then the
    copy's pages and file bytes once planned, before any S3 copy.
  - Import: every page and the declared file bytes, once, before the first
    create; over → refused whole.
  - Blob upload: + the file's size.
- Refusal: `403`,
  `{"error":{"code":"quota.exceeded","message":"…","details":{"quota":"pages"|"blob_bytes","scope":"user"|"workspace","limit":N}}}`,
  the counter `knot_quota_exceeded_total{quota,scope}`, and the throttled
  audit event (`target_kind = "user"`, data `{quota, scope, limit, used,
  requested}`).
- **Best effort:** concurrent creates can both pass; the overshoot is
  bounded by what runs at once. Documented. A strict check would serialise
  every create behind one lock. Imports overshoot the most: one user may
  start up to the `expensive` burst (10) at once per replica, each checked
  against the same usage, so a quota can be passed by up to 10 × the
  import caps (10 000 pages, 512 MiB of files) per replica.
- One ordinary migration adds `documents (workspace_id, created_by)` and
  `blobs (workspace_id, created_by) INCLUDE (byte_size)` (small tables;
  `knot_lock_tables` bounds the lock wait).
- The README warns that a workspace quota without a per-user one lets one
  account use it all.

### Memory

Every limiter map (the new ones and the login throttle) prunes itself:
every 1 024 inserts, buckets that are full again (idle) are removed, and a
map never holds more than 100 000 keys. Past that, the HTTP and collab
maps give new keys one shared overflow bucket (a full map is pruned first
when an at-cap prune is due; where a new key's state goes, its own slot or
the overflow bucket, is decided from the map its state was read from, so a
prune during the store never hands a key the overflow bucket's debt). The
login throttle has no overflow bucket, since a shared one would let anyone
who fills the map lock everyone out of sign-in: a failure for a new key on
a full map drops the drained keys and, when that frees nothing, evicts the
sixteenth of the keys whose last failure is oldest; a key without a bucket
has no failures and is allowed. A key is so forgotten only after about
100 000 newer failures elsewhere. The login throttle's email key is capped
at 320 bytes. No background task.

### Operator surface

- Helm: `rateLimit.{enabled,scale,collabMaxSockets}`, `trustedProxies` (a
  list or a comma string, rendered with `join`), `quotas.*`; every number
  rendered `| int64 | quote` (byte values otherwise render as `1.6e+07`,
  which figment refuses); schema entries (quotas at most 2^53 − 1, since
  Helm holds values-file numbers as float64; `trustedProxies` limited to IP
  and CIDR characters, with any `/0`, zero-padded too, refused; knot checks
  the rest at startup); README section, upgrade note,
  Replicas note, alert `KnotRateLimitedSustained` (ticket severity, on a
  sustained 429 rate), and a chart check that the rendered ConfigMap has no
  `e+`.
- Metrics: `knot_rate_limited_total{bucket}` (WS upgrade refusals as
  `collab_upgrade` and `collab_sockets`), `knot_collab_throttled_total{kind,action}`,
  `knot_collab_throttle_wait_seconds`, `knot_quota_exceeded_total{quota,scope}`,
  `knot_client_ip_unresolved_total{reason}`,
  `knot_client_ip_malformed_forwarded_total`; a Grafana "Limits" row.
- README states the remaining fill rates (collab bytes, board SVGs, and
  without quotas duplicates and imports) so operators know quotas cover
  pages and files only, and that clients behind an L4 balancer without
  `X-Forwarded-For`, or behind one NAT or egress address, share one
  sign-in bucket.

### Client

- One helper `limitErrorText(error)` turns `rate.limited`
  (`details.retry_after_secs`) and `quota.exceeded` into text ("You're
  doing that too fast. Try again in 7 s.", "This workspace has reached its
  page limit.", …). User-initiated actions show it as a warning toast in
  place of their generic error (ticking a task on the Tasks page included,
  which also puts the checkbox back). Background writes (board SVG,
  notification read, what's-new seen) stay silent.
- Toasts take an optional dedupe key; a second rate-limit toast replaces
  the first instead of stacking.
- Toasts may also ask for their own duration in place of the default 4 s.
- Multi-file drop: on `rate.limited`, wait `retry_after_secs` (at most
  30 s) and retry that file once; a wait over 2 s first shows "Uploads
  paused — continuing in N s…" under the same dedupe key, for the whole
  wait, which the final toast replaces; on `quota.exceeded`, stop with one
  toast saying how many files weren't uploaded. Leaving the doc (or the
  editor unmounting) aborts the drop: nothing more is uploaded or
  inserted, and a pause notice still showing becomes "Uploads stopped. N
  files weren't uploaded."
- Sign-in, the public share page and the workspace import show the
  rate-limit text; the two `<a download>` zip exports become
  fetch-and-save so their 429 can be shown.
- No retries are added; nothing retries a 429 today.

## Tests (behaviours, each written red first)

1. Unit: bucket refill, burst ceiling, round-up of `retry_after_secs`,
   debt and its wait, scale and floors, pruning, the key cap and overflow;
   the login throttle's eviction (a full map never refuses a new key).
2. Unit: `X-Forwarded-For` ignored unless the peer is trusted; right-most
   untrusted hop; a spoofed left-most entry; several header lines; ports
   and brackets; mapped IPv6; all-trusted chain → right-most entry;
   malformed entries; `/64` grouping.
3. Integration: after `burst` writes the next is 429 with
   `retry_after_secs`; another user is unaffected; reads aren't limited;
   HEAD on an export is limited like GET; each named bucket is separate; a
   401 or a CSRF failure costs nothing; limits off → never 429.
4. Integration: sign-in limited per client IP; two clients behind a trusted
   proxy get separate buckets; an untrusted peer sending XFF skips the IP
   buckets and is counted.
5. Integration (WS): a burst of updates past the byte budget is fully
   persisted and reaches a second client, only later; a revoke during a
   wait closes with 4403 promptly; awareness past budget is coalesced (the
   last state arrives, per awareness client); a client that vanishes during
   a wait still gets its already-sent edits applied; a client that reads
   nothing for the silence limit is let go; a second `SyncStep1` gets no reply; the 65th socket
   and a reconnect loop get 429.
6. Integration: each quota refuses with 403 `quota.exceeded`, writes
   nothing, audits once per window; quotas off → nothing counted. Import
   caps: too many pages, too many bytes, an entry referenced many times (an
   attachment or a page's Markdown), an entry larger than declared.
7. Unit: config defaults, validation messages naming the env var, env
   parsing; metric descriptions. Chart: rendered numbers have no `e+`.
8. Vitest: the text helper, each wired call site, the upload loop, toast
   dedupe, the zip exports' error path.
9. e2e: the whole suite passes on the default limits.

## Left to the builder

- Module names and file layout; the token-bucket arithmetic.
- Alert threshold value.
