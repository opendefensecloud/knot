# Outcome: rate limits and quotas for writes

**Date:** 2026-11-12
**Issue:** #88
**Spec:** [specs/2026-11-12-rate-limits-design.md](../specs/2026-11-12-rate-limits-design.md)
**Plan:** [plans/2026-11-12-rate-limits.md](../plans/2026-11-12-rate-limits.md)
**Goal:** Bound how fast one signed-in user (or one anonymous client address) can write, and optionally how much a user or the workspace stores, without ever losing a collab edit or tripping on normal use.

## What landed

- **Config** (`knot-config`). `rate_limit_enabled` (on), `rate_limit_scale` (1.0, in (0, 100]), `rate_limit_collab_max_sockets` (64), `trusted_proxies` (empty) and four quotas (`quota_{pages,blob_bytes}_per_{user,workspace}`, 0 = off). `trusted_proxies_list()` refuses `/0` (and `::ffff:0:0/96`), host bits after the prefix and IPv4-mapped CIDRs below /96, and maps `::ffff:a.b.c.d/96+n` to IPv4. Every error names the env var.
- **Token bucket** (`knot-server/src/rate_limit/bucket.rs`). `KeyedBuckets<K>` with `try_take`, `charge` (debt allowed), `deficit` and `time_until_tokens`, on `tokio::time::Instant`. Each map prunes full (idle) buckets every 1 024 new keys and caps itself at 100 000 keys, with one shared overflow bucket past that and an at-cap prune so idle keys can't pin new keys on the overflow. No background task. The login throttle (`knot-auth/src/throttle.rs`) got the same pruning and cap, plus 320-byte keys, but no overflow bucket (a shared one would let whoever fills the map lock everyone out of sign-in): on a full map it evicts the keys whose last failure is oldest.
- **Client IP** (`knot-server/src/client_ip.rs`). `X-Forwarded-For` is read only from a trusted peer, right to left, skipping trusted hops. If every hop is trusted, the right-most entry is the client. A malformed entry stops the walk. IPv6 is keyed per `/64`. The result feeds the per-IP buckets, the login and password failure throttles and the IP stored on a local session.
- **HTTP limits** (`rate_limit/http.rs`). `classify(method, MatchedPath)` puts each request in exactly one of seven buckets: `expensive`, `export`, `upload`, `board_preview` and `write` per user; `sign_in` and `public` per client IP. HEAD counts as GET, and any unlisted mutating `/api` route falls into `write`. The `/api` layer runs after session and CSRF and before the doc-role check. `/auth/*` and `/p/*` get their own `anonymous_mw`. `/auth/password` charges `expensive` in its handler (`take_user_token`), only for a JSON body (a same-site form can't drain the bucket duplicate and import share). A refusal is a 429 `rate.limited` with `Retry-After` and `details.retry_after_secs` (`http_error::rate_limited`); a browser opening `/p/{token}` (`Accept: text/html`) gets an HTML 429 page instead (`http_error::rate_limited_page`).
- **Collab WebSocket** (`rate_limit/collab.rs`, `room.rs`, `board_room_shim.rs`).
  - Budgets per user × {doc, board}: bytes and messages enforced by backpressure (wait before the next read, never drop).
  - Awareness is coalesced to the newest state per awareness client on each socket (held frames are merged by client clock).
  - When the client is gone during a wait, the edits already readable are forwarded before the session ends.
  - With limits on, the writer pings every 30 s and a session silent for 75 s ends, so half-open sockets free their place under the socket cap (`Liveness`). The writer abandons a send that hangs on such a socket once the read loop has ended, so a busy room's full send buffer can't hold the session until TCP gives up.
  - A second client `SyncStep1` is ignored.
  - At upgrade, a per-user socket cap (`SocketGuard`) and a per-user × target upgrade bucket (20, then 20 a minute) refuse with 429 (socket cap `Retry-After` 5 s).
  - The board shim now gets the user id.
- **Import caps** (`routes/api/export_import.rs`). Always on: 10 000 pages, 256 MiB of page Markdown and 512 MiB of attachment and board bytes (declared sizes summed per reference), and 20 000 attachment and board references. All are checked before the first create, and going over any is a 413 `import.too_large` with `{pages, bytes, markdown_bytes, references, max_*}`. Each entry is read with a limit of its declared size + 1. Skips are logged at debug, with one warn per import (`skipped_entries`). `ImportPlan` feeds the quota check.
- **Quotas** (`knot-storage/src/quota.rs`, `knot-server/src/quota.rs`, migration `20261112120000_quota_indexes`).
  - Pages and file bytes are counted per user (`created_by`) and per workspace, archived rows included.
  - They are checked before writing in create, from-template, duplicate (pre-check, then the planned copy), import and blob upload.
  - A refusal is a 403 `quota.exceeded` with `{quota, scope, limit}`, plus `knot_quota_exceeded_total{quota,scope}` and an audit row (`audit::record_with_data`) at most once per user, quota and scope every 10 minutes.
  - The migration adds `documents (workspace_id, created_by)` and `blobs (workspace_id, created_by) INCLUDE (byte_size)` under `knot_lock_tables`.
- **Metrics** (`knot-obs`, `describe_limit_metrics()`): `knot_rate_limited_total{bucket}`, `knot_collab_throttled_total{kind,action}`, `knot_collab_throttle_wait_seconds`, `knot_quota_exceeded_total{quota,scope}`, `knot_client_ip_unresolved_total{reason}` and `knot_client_ip_malformed_forwarded_total`.
- **Web.**
  - `limitErrorText` and `notifyFailure` (`web/src/lib/limitErrors.ts`) turn `rate.limited` and `quota.exceeded` into one warning toast, under a dedupe key (`notify(kind, text, { key })`). Every user-initiated write in the spec's list uses it; background writes stay silent.
  - The multi-file drop (`features/editor/uploadFiles.ts`) waits up to 30 s and retries once on 429, showing "Uploads paused — continuing in N s…" for the whole of a wait over 2 s (toasts take an optional `durationMs`). On a quota it stops with one toast. Moving to another doc or unmounting the editor aborts the drop, so no later file goes to the old doc or into the new doc's editor.
  - Ticking a task on the Tasks page now shows the limit text (or "Couldn't update the task.") and puts the checkbox back; before, any refusal left the optimistic tick with no message.
  - The page and workspace zip exports are now fetch-and-save (`features/export/downloadFile.ts`).
  - Sign-in, the public share page and the workspace import show the limit text. The import also names the caps from the 413 details.
- **Operator surface.**
  - Helm values: `rateLimit.{enabled,scale,collabMaxSockets}`, `trustedProxies` (a list or a string), `quotas.*` and `alerting.rateLimitedPerSecond`. Numbers render with `| int64 | quote`.
  - Schema guards: quotas at most 2^53 − 1; `trustedProxies` limited to IP/CIDR characters, with any `/0` refused, zero-padded too.
  - Alert `KnotRateLimitedSustained`, a Grafana "Limits" row, and a new "Rate limits and quotas" section in the chart README with an upgrade note and Replicas notes.
  - Also updated: `docs/SLO.md`, `SECURITY.md` and `.env.example`.
  - `deploy/helm/check-render.sh` runs in `make helm.check` and in `helm-ci.yaml`.

## Decisions on the issue's open questions

- **Per user or per workspace?** Rate limits are per user, and per client IP on anonymous routes. knot runs one workspace per instance, so a per-workspace rate would be one bucket for everyone. Quotas exist at both levels. The README warns that a workspace quota without a per-user one lets one account use all of it.
- **Owners exempt?** No. A compromised owner is the worst case, the defaults are far above normal use, and the operator (not the owner) sets them.
- **Audit or notify on quota?** A quota refusal is audited as `quota.exceeded`, at most once per user, quota and scope every 10 minutes per replica, so a script at its quota can't flood the audit log. A 429 is never audited. It goes to the counter and to one log line when a key *starts* being limited. Owner notifications are deferred.
- **Collab per-connection limits vs fast typing?** The budgets are per user and enforced by backpressure: the server delays reading, nothing is dropped, and no new close code is added. Doc and board budgets are separate, so a board drag never slows typing. Defaults leave wide headroom: typing is ≤ 20 KB/s against a 1 MiB/s budget with a 32 MiB burst.
- **Multiple replicas?** Buckets, the login throttle, the socket cap and the quota-audit window live in each replica's memory. A user spread over N replicas gets up to N × each limit, and `rateLimit.scale` divides the HTTP and collab budgets, though not the login throttle or the socket cap. A shared store would cost a Postgres round trip per write and per WS frame. Quotas are counted in Postgres and hold across replicas (best effort under concurrency).

## Deviations from the spec and plan

The spec and plan were updated to match each of these.

- **Stricter `trusted_proxies` parsing.** Plan Task 1 only refused a prefix of 0. A review found that `10.0.0.5/8` was silently widened, and that IPv4-mapped CIDRs could trust everyone (`::ffff:0:0/96`) or be read wrongly. Host bits are now refused, with a suggested network form. Mapped CIDRs of /96 or longer become IPv4, and shorter ones are refused.
- **`knot_client_ip_unresolved_total` has a `reason` label.**
  - `no_trusted_proxies`: the per-IP bucket is skipped.
  - `untrusted_peer`: proxies are configured but the peer isn't one, so the peer is limited as the client. This can be a direct client sending the header, so it is no sure sign of a missing proxy.
  - Each reason logs its own once-per-process warning.
  - The Grafana panel became `sum by (reason)`.
  - `knot_client_ip_malformed_forwarded_total` was added for the spec's "malformed entry is counted".
- **Import reference cap.** Added in review: `MAX_IMPORT_REFERENCES = 20_000`. Before it, an archive with many references could make planning cost O(references × entries) through the zip crate's linear name fallback. Zip entries are now looked up by exact name (`index_for_name_raw`), which drops the CP437 name fallback. That makes no difference for knot exports, whose paths are ASCII slugs.
- **Import skip reporting.** The plan logged each skipped entry at warn. Each skip is now a debug line, and one warn per import carries `skipped_entries`. Entries declaring more than `MAX_ENTRY_BYTES` are now counted too; before, they were skipped without any log.
- **Import quota bytes.** These are the declared attachment bytes only. Board files aren't blobs and don't count, matching "board SVGs don't count".
- **Quota audit window.** It lives in memory per replica, so with N replicas a refusal can be audited up to N times per window. A failed audit write still uses up the window, because the slot is taken before the write.
- **Web additions beyond the plan.**
  - The "Uploads paused" notice was added to the spec's Client section.
  - The upload loop moved out of `KnotEditor.tsx` into `uploadFiles.ts`, so it could be tested.
  - `downloadFile.ts` wraps the existing `saveBlob`.
  - `CopyLinkButton` and the tree's move error also go through `notifyFailure`.
  - The workspace import's 413 text names the caps.
- **Helm schema.** Values-file numbers are float64 in Helm, so quotas are capped at 2^53 − 1. `--set rateLimit.scale=0.5` is refused as a string (checked with helm 4.3.1), so the docs say to use a values file or `--set-json`.
- **Test layout.** A non-DB integration target, `tests/collab_throttle_loop.rs`, runs the real `room::serve` / `board_room_shim::serve` loops over sockets with in-memory stores. It covers spec test 5's loop behaviours without Postgres, and it uses deadline-based waits, not fixed sleeps. The DB-backed `collab_rate_limit_integration.rs` keeps the full-stack and upgrade tests.
- **Small ones.**
  - The migration's `knot_lock_tables` caller name is `'quota_indexes'`.
  - The plan's `refuse_user` is `take_user_token`.

## Gates

All run on the final tree.

- **Rust:**
  - `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings` (the CI command): clean.
  - `cargo test --workspace` against a throwaway Postgres 18 (tmpfs, own compose project): everything passes. The first run had 1702 passed and 89 failed:
    - 86 failures came from the throwaway database's tmpfs filling up (`could not extend file … No space left on device`), all in knot-storage. After recycling the container, `cargo test -p knot-storage` passed 303/303.
    - 3 failures were real, in `collab_rate_limit_integration.rs`: its fixture created every extra page with the same top-level sort key, so the second `create` hit the unique constraint (`Conflict`). The fixture now hands out increasing keys, and the target passes 8/8.
  - The timing-sensitive targets ran three more times, green every time: `collab_rate_limit_integration` (8), `collab_throttle_loop` (15), `rate_limit_integration` (20), `quota_integration` (12).
  - The suites that guard "existing tests unaffected" pass: `offline_sync`, `restricted_realtime_integration`, `duplicate_integration` (including the 199-create test), `workspace_import_integration`, `migrations_apply` and the board tests. The new migration applies.
- **Web** (from `web/`): `NODE_OPTIONS=--no-experimental-webstorage npx pnpm vitest run` (122 files, 1261 tests), `npx pnpm tsc --noEmit` and `npx pnpm lint` all pass.
- **Helm:** `deploy/helm/check-render.sh`, `make helm.check` and `shellcheck deploy/helm/check-render.sh` pass. The new schema cases were checked red against the old schema on a scratch copy.
- **e2e, on the default limits:** Playwright ran against a Docker stack with knot built from the repo `Dockerfile`, tmpfs Postgres, Dex, toxiproxy and Vite, and knot left at its default config (limits on, as on CI). 142 tests passed. The two `ws-reconnect` tests failed only because toxiproxy was missing from that first stack; with toxiproxy added they passed, together with `collab`, `two-users-converge`, `comments-realtime`, `excalidraw` and `restricted` (9/9). The whole suite signs in through Vite from one address and still passes with the default `sign_in` bucket.

## Non-obvious

- `AppState::in_memory()` / `with_pool()` turn rate limits off, and `router_with_state` builds the limiter from `state.config`. Existing tests therefore run unlimited, and production can't forget the wiring: `main` already installs the operator's config. A test that wants limits must set the config before building the router.
- Without trusted proxies, a request that carries `X-Forwarded-For` **skips** the per-IP buckets instead of sharing one bucket among everyone behind the ingress. "Never worse than today" mattered more than catching a direct client that adds the header to escape its bucket: that client only gets back today's behaviour.
- Backpressure only shows in the client once kernel and proxy buffers (several MiB) are full, so "Saving…" appears late and only under a sustained flood. A socket waiting off debt doesn't read, so it sees the client's Close only after the wait. A read waits only while the user's budget is in debt, so one socket owes at most the one frame it read last (≤ 4 MiB): about 4 s for a doc and 1 s for a board at default scale, up to 64 s at the byte floor; several sockets of one user flooding at once add up. Revoke, refusal, room end and shutdown still end the wait at once.
- Helm turns values-file numbers into float64. Without `| int64 | quote`, a 10 GiB quota renders as `1.073741824e+10`, which figment refuses at startup. `check-render.sh` asserts that no `e+` appears.
- An export is a GET, and a cross-site navigation carries the Lax cookie. Exports therefore have their own bucket, so a hostile page can drain `export` but not `write` or `expensive`.

## Deferred

- Owner notifications for quota refusals; a usage UI.
- A shared, cross-replica rate-limit store.
- Per-bucket overrides in config; quotas on CRDT history bytes or board SVGs; a per-document CRDT size cap.
- An `Origin` check on the collab upgrade (separate hardening issue).

## Carryforward

- **Unrelated stale docs** the ops pass left alone:
  - `SECURITY.md` still says Excalidraw boards have no cross-pod fan-out, but `values.yaml` says they fan out.
- **Socket cap after a flood:** a user who floods and then reloads many tabs can hold their socket-cap places for the length of the wait. If that shows up in practice, read the Close while waiting.
