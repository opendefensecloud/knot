# Rate Limits and Quotas Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Per-user and per-IP rate limits on knot's writes (HTTP and collab WebSocket), always-on import caps, and optional page / file-byte quotas, with config, Helm values, metrics and in-app messages (issue #88).

**Architecture:** A new `crates/knot-server/src/rate_limit/` module holds an in-memory keyed token bucket (self-pruning, key-capped) and three users of it: an HTTP classifier + middleware (by method and `MatchedPath`), the collab limits used by the two WS read loops (backpressure, presence coalescing, socket cap, upgrade bucket), and nothing else. `client_ip.rs` resolves the client address through an operator-trusted proxy list. A `quota` module counts pages and blob bytes in Postgres before writes. Limits are built from `state.config` inside `router_with_state`; the `AppState` constructors turn them off so existing tests are unaffected, and `main` already installs the operator's config.

**Tech Stack:** Rust (axum 0.8, tokio, sqlx 0.9, figment, ipnet 2), Postgres 18, Helm, React 18 + Vitest.

**Spec:** [docs/superpowers/specs/2026-11-12-rate-limits-design.md](../specs/2026-11-12-rate-limits-design.md) — binding. Where this plan and the spec disagree, the spec wins; say so in the commit message.

**Base branch:** `feat/rate-limits` off `origin/main` (5ec52a4), worktree `/home/chris/odc/knot-wt-rate-limits`.

## Global Constraints

- Error envelope `{"error":{"code","message","details"}}` via `http_error.rs`; add `json_err_with_details(status, code, message, details: serde_json::Value)` and `rate_limited(retry_after_secs: u64)` (429, `Retry-After`, `details.retry_after_secs`).
- Bucket constants exactly as the spec's tables (HTTP: expensive 10/10, export 20/20, upload 60/60, board_preview 300/300, write 120/120, sign_in 30/30, public 600/600 — burst, per minute. WS: doc bytes 32 MiB / 1 MiB/s, board bytes 64 MiB / 4 MiB/s, doc msgs 1000 / 300/s, board msgs 2000 / 600/s, awareness 240 / 120/s per user × kind, upgrades 20 / 20 per min per user × target, max sockets from config). `rate_limit_scale` multiplies burst and rate; floors: every burst ≥ 1 token and rate ≥ 1/min; WS bytes rate ≥ 64 KiB/s and burst ≥ 4 MiB; WS msgs ≥ 20/s.
- Metric labels never carry user ids or IPs. Metric names: `knot_rate_limited_total{bucket}`, `knot_collab_throttled_total{kind,action}`, `knot_collab_throttle_wait_seconds`, `knot_quota_exceeded_total{quota,scope}`, `knot_client_ip_unresolved_total{reason}`. Describe each in `crates/knot-obs/src/metrics.rs` in its own `describe_limit_metrics()` with a test like `history_metrics_are_described`.
- No background tasks; maps prune themselves.
- Time comes from `tokio::time::Instant` so tests use `#[tokio::test(start_paused = true)]`.
- Migrations forward-only; bound lock waits with `knot_lock_tables` (see `migrations/20261109120000_doc_short_links.sql`). After adding one, `touch crates/knot-test-support/src/lib.rs`.
- Tests: `knot_test_support::fresh_db()`, never testcontainers. DB-backed tests need Postgres on `KNOT_TEST_DATABASE_URL` (default `127.0.0.1:5432`). If none is reachable, still write them, make them compile (`cargo test --workspace --no-run`), run every non-DB test, and say in the task report which tests were not run.
- Local toolchain differs from CI: clippy with `cargo clippy --workspace --all-targets -- -D warnings -A clippy::result_large_err -A deprecated`; `cargo test` (nextest is not installed); vitest with `NODE_OPTIONS=--no-experimental-webstorage` and pnpm via `npx pnpm`.
- **Do not commit or push** (the session's permissions refuse it; the user commits). Leave each task's changes in the working tree; don't run `git add`, `git stash`, `git checkout -- …` or `git reset` either — other agents may be working in `web/` or `deploy/` at the same time. The commit messages named in each task are for the user's later commits.
- Descriptive variable names (user rule).

## Review Focus

1. **An edit is never lost under WS backpressure** → Task 4, `ws_burst_past_byte_budget_is_persisted_and_delivered_later`.
2. **A revoke during a throttle wait still closes 4403 promptly** → Task 4, `revoke_during_throttle_wait_closes_promptly`.
3. **Unconfigured proxy: no shared IP bucket** → Task 3, `untrusted_peer_with_forwarded_header_skips_ip_buckets`.
4. **Spoofed `X-Forwarded-For`** → Task 2 unit tests in `client_ip.rs`.
5. **HEAD bypass** → Task 3, `head_on_export_is_limited_like_get`.
6. **Existing tests unaffected** (constructors disabled) → Task 3, `constructors_build_state_without_limits` and the full suite.
7. **Helm numbers render as integers** → Task 8 chart render check.
8. **Import writing more than it checked** → Task 5, `entry_larger_than_declared_is_skipped`.

---

### Task 1: Config — fields, defaults, validation

**Files:** Modify `crates/knot-config/src/lib.rs`, `crates/knot-config/Cargo.toml` (add `ipnet.workspace = true`; add `ipnet = "2"` to root `Cargo.toml` `[workspace.dependencies]`, it is already in `Cargo.lock`), `crates/knot-config/tests/load.rs`.

**Produces:**
- Fields (flat, `#[serde(default)]`): `rate_limit_enabled: bool` (true), `rate_limit_scale: f64` (1.0), `rate_limit_collab_max_sockets: u32` (64), `trusted_proxies: String` ("", `deserialize_with = "de_string_or_number"`), `quota_pages_per_user: u64`, `quota_pages_per_workspace: u64`, `quota_blob_bytes_per_user: u64`, `quota_blob_bytes_per_workspace: u64` (all 0 = off). Doc comments in the style of the `history_*` group.
- `pub fn trusted_proxies_list(&self) -> Result<Vec<ipnet::IpNet>, ConfigError>`: split on `,`, trim, skip empty; a bare IP becomes `/32` or `/128`; a prefix length of 0 is refused; errors name `KNOT_TRUSTED_PROXIES`.
- `validate()`: scale `> 0 && <= 100` (NaN fails) → `"KNOT_RATE_LIMIT_SCALE must be > 0 and <= 100"`; `rate_limit_collab_max_sockets >= 1`; `trusted_proxies_list()` must succeed.

- [ ] Write failing unit tests (defaults; each invalid value with its message; `0.0.0.0/0` and `::/0` refused; `10.0.0.0/8, 192.168.1.5 ,fd00::/8` parses to three nets; env parsing through `figment::Jail` in `tests/load.rs` incl. `KNOT_QUOTA_BLOB_BYTES_PER_WORKSPACE=10737418240` and `KNOT_RATE_LIMIT_SCALE=0.5`).
- [ ] Implement; `cargo test -p knot-config`; clippy; commit `feat(config): rate-limit, trusted-proxy and quota settings (#88)`.

### Task 2: Core — keyed token bucket, client IP, login-throttle hygiene

**Files:** Create `crates/knot-server/src/rate_limit/mod.rs`, `rate_limit/bucket.rs`, `crates/knot-server/src/client_ip.rs`; modify `crates/knot-server/src/lib.rs` (modules), `crates/knot-server/Cargo.toml` (`ipnet`), `crates/knot-auth/src/throttle.rs`.

**Produces (`rate_limit::bucket`):**
- `#[derive(Clone, Copy, Debug, PartialEq)] pub struct Rate { pub burst: f64, pub per_second: f64 }` with `Rate::per_minute(burst: u32, per_minute: u32)`, `Rate::per_second(burst, per_second)`, `fn scaled(self, scale: f64, floor: Rate) -> Rate` (component-wise max with the floor).
- `pub struct KeyedBuckets<K> { rate: Rate, inner: Mutex<Inner<K>> }` (std `Mutex`, short critical sections, never held across `.await`), `K: Hash + Eq + Clone`:
  - `fn try_take(&self, key: &K, cost: f64, now: Instant) -> Result<(), Duration>` — refill (`tokens = min(burst, tokens + elapsed × rate)`), take if `tokens >= cost`, else `Err(time until tokens >= cost)` (for `cost > burst`, the time until full).
  - `fn charge(&self, key: &K, cost: f64, now: Instant)` — refill, subtract (may go negative).
  - `fn deficit(&self, key: &K, now: Instant) -> Duration` — zero when `tokens >= 0` after refill, else `-tokens / rate`.
  - `fn time_until_tokens(&self, key, needed, now) -> Duration` for the awareness coalescer.
  - Pruning: every 1 024 inserts of a new key, `retain` buckets that are not full after refill. Cap 100 000 keys: past it (after a prune), a new key maps to one shared overflow bucket (`enum Slot<K> { Key(K), Overflow }`). Every lookup of a key without its own bucket on a full map (take, refusal, charge or read; the throttle's `check` too) first prunes when an at-cap prune is due (64 such lookups or 1 s since the last prune), so idle keys left by an attack never keep new keys on the overflow bucket. `store` decides own slot vs. overflow from the map state `peek` read, before its periodic prune, so a prune during the store never moves overflow state (debt) into a key's own slot. `fn len(&self) -> usize` for tests.
- `pub fn retry_after_secs(wait: Duration) -> u64` — `ceil`, at least 1.

**Produces (`client_ip`):**
- `pub struct TrustedProxies(Vec<IpNet>)` with `from_config(&Config) -> Self` (logs and returns empty on a parse error; config validation already refuses it in prod) and `is_empty()`, `contains(IpAddr)` (after `to_canonical()`).
- `pub struct ClientIp { pub ip: IpAddr, pub resolved: bool }` — `resolved == false` exactly when the peer is **not trusted** and the request carries `X-Forwarded-For` (an unknown proxy in front).
- `pub fn resolve(peer: Option<IpAddr>, headers: &HeaderMap, trusted: &TrustedProxies) -> ClientIp` — the spec's algorithm: all XFF lines in order, comma split, trim, `ip` / `ip:port` / `[v6]` / `[v6]:port`, canonicalise; right-to-left skipping trusted; all trusted → right-most entry; malformed → last valid address read, else peer. Missing peer (tests via `oneshot`) → `0.0.0.0`.
- `pub fn from_request(req: &Request, trusted: &TrustedProxies) -> ClientIp` reading `ConnectInfo<SocketAddr>`.
- `#[derive(Hash, Eq, PartialEq, Clone, Copy, Debug)] pub struct IpKey(IpAddr)` with `IpKey::of(ip)` — IPv4 as is, IPv6 masked to `/64`.

**knot-auth `Throttle`:** prune drained keys every 1 024 inserts (a key whose drained count is 0 is removed), cap 100 000 keys (no overflow bucket, which would let a full map lock everyone out of sign-in: a failure for a new key on a full map drops the drained keys, then evicts the sixteenth of the keys whose last failure is oldest; a key without a bucket is allowed), and callers cap email keys at 320 bytes (Task 3 does the caller side; add a `pub const MAX_KEY_BYTES: usize = 320` and truncate inside `record_failure`/`check` too).

- [ ] Failing unit tests: refill, burst ceiling, `try_take` wait values, cost > burst, debt + `deficit`, scale + floors, prune keeps active / drops idle, key cap → overflow (HTTP/collab maps) or eviction (throttle), `retry_after_secs` rounding; client IP cases from spec test 2; throttle prune and key truncation.
- [ ] Implement; `cargo test -p knot-server --lib rate_limit client_ip`, `cargo test -p knot-auth`; clippy; commit `feat(server): keyed token bucket and trusted-proxy client IP (#88)`.

### Task 3: HTTP limits

**Files:** Create `crates/knot-server/src/rate_limit/http.rs`; modify `rate_limit/mod.rs`, `lib.rs` (`AppState.rate_limits: Arc<RateLimits>`, `AppState.trusted_proxies: Arc<TrustedProxies>`, both built from `state.config` in `router_with_state` before any clone; constructors set `config: Arc::new(Config { rate_limit_enabled: false, ..Default::default() })` and disabled limits), `http_error.rs`, `routes/api/mod.rs` (layer innermost, after csrf), `lib.rs` (anonymous layer on `routes::auth::router()` and `routes::public::router()` merges), `routes/auth/local.rs` (login + password throttles keyed by `IpKey` of the resolved client IP; session IP from it; `/auth/password` charges `expensive` after the session check; email key capped), `crates/knot-obs/src/metrics.rs`; tests `crates/knot-server/tests/rate_limit_integration.rs`.

**Produces:**
- `pub struct RateLimits { pub enabled: bool, pub http: HttpLimits, pub collab: CollabLimits /* Task 4; a stub with `disabled()` until then */ }`, `RateLimits::from_config(&Config)`, `RateLimits::disabled()`.
- `pub enum HttpBucket { Expensive, Export, Upload, BoardPreview, Write, SignIn, Public }`, `label() -> &'static str` (`"expensive"`, `"export"`, `"upload"`, `"board_preview"`, `"write"`, `"sign_in"`, `"public"`), `rate() -> Rate`.
- `pub fn classify(method: &Method, matched_path: &str) -> Option<HttpBucket>` — HEAD as GET; explicit tables for the named templates; any other POST/PUT/PATCH/DELETE whose path starts with `/api/` → `Write`; else `None`. `/auth/password` is not classified here (handler-charged).
- `pub async fn api_mw(State(state), req, next)` — needs `AuthContext` (absent → pass through), `MatchedPath`; key user id; on refusal `rate_limited(secs)`, `knot_rate_limited_total{bucket}` +1, log once when the key starts being limited (track per-key "was limited" in the bucket state or a small side map; never per request).
- `pub async fn anonymous_mw(...)` — `SignIn`/`Public` only; key `IpKey` of `client_ip::from_request`; `resolved == false` → `knot_client_ip_unresolved_total{reason}` +1 and `tracing::warn!` once per process and reason (an `AtomicBool` each); `reason="no_trusted_proxies"` (trusted_proxies empty) skips the bucket, `reason="untrusted_peer"` (proxies configured, peer not among them) limits the peer as the client. The spec counts a malformed `X-Forwarded-For` entry: when `ClientIp::malformed_forwarded_entry` is set, count it once per request (a separate counter, e.g. `knot_client_ip_malformed_forwarded_total`, described with the other limit metrics). Note that an empty entry (an empty header line, a trailing comma) is malformed too.
- `pub fn HttpLimits::check(&self, bucket, key, now) -> Result<(), Duration>`.

- [ ] Failing tests (unit: `classify` table incl. HEAD, every explicit template present in the real router — build `router()` and assert each template answers something other than 404 for a request with matching method, or assert via a list compiled from route modules; integration, DB-backed: spec test 3 and 4, `constructors_build_state_without_limits`, metrics description test).
- [ ] Implement; run unit + integration tests; clippy; commit `feat(server): per-user and per-IP HTTP rate limits (#88)`.

### Task 4: Collab WebSocket limits

**Files:** Create `crates/knot-server/src/rate_limit/collab.rs`; modify `lib.rs` (`collab_upgrade`, `collab_board_upgrade`: socket cap + upgrade bucket before the ACL check, 429 via `rate_limited`; pass `ctx.user_id` and the limits to the board shim), `room.rs`, `board_room_shim.rs`; tests `crates/knot-server/tests/collab_rate_limit_integration.rs` (raw tungstenite clients as in `tests/offline_sync.rs`; `close_code_within` pattern from `tests/restricted_integration.rs`).

**Produces:**
- `pub enum CollabKind { Doc, Board }`.
- `pub struct CollabLimits` (disabled form: every check passes, `deficit` is zero) with:
  - `fn open_socket(&self, user_id, kind, target_id, now) -> Result<SocketGuard, Duration>` — upgrade bucket per `(user, target)`, then the open-socket count per user (`Mutex<HashMap<Uuid, usize>>`; `SocketGuard` decrements on drop; refusal → `Err(Duration::from_secs(5))`). Refusals count `knot_rate_limited_total{bucket="collab_upgrade"|"collab_sockets"}`.
  - `fn charge_frame(&self, user_id, kind, bytes: usize, is_awareness: bool, now)` and `fn deficit(&self, user_id, kind, now) -> Duration` (max of bytes and msgs).
  - `fn try_take_awareness(&self, user_id, kind, now) -> Result<(), Duration>`.
- Read loop per the spec's pseudocode, in both shims, factored into a shared helper where it reduces duplication (e.g. `async fn wait_for_budget(limits, user, kind, cancel tokens…) -> WaitOutcome { Continue, Stop }`). The wait selects on shutdown, revoked, refused, room handle shutdown and done; sleeps `min(deficit, 1 s)` and re-checks; records `knot_collab_throttle_wait_seconds` once per wait and `knot_collab_throttled_total{kind="bytes",action="delayed"}`.
- Awareness coalescer per connection: `pending_awareness: Option<Vec<u8>>`; over budget → store (replacing; `knot_collab_throttled_total{kind="awareness",action="coalesced"}`); a `sleep_until` select arm forwards it when a token is available.
- `SyncStep1`: answered once per connection (today's behaviour); later ones ignored, `knot_collab_throttled_total{kind="sync_step1",action="ignored"}`.

- [ ] Failing tests (unit, paused time: `CollabLimits` arithmetic, socket guard, upgrade per target; integration: spec test 5 — burst persisted and delivered, revoke during wait → 4403 within 2 s, awareness coalesced so the last state arrives, second `SyncStep1` unanswered, 65th socket → 429, reconnect loop → 429; existing `offline_sync`, `restricted_realtime_integration` and board tests still pass).
- [ ] Implement; tests; clippy; commit `feat(server): collab WebSocket backpressure, presence coalescing and socket caps (#88)`.

### Task 5: Import caps

**Files:** Modify `crates/knot-server/src/routes/api/export_import.rs`; tests in `crates/knot-server/tests/` (the existing import test file; find it with `grep -l workspace/import crates/knot-server/tests`).

- `const MAX_IMPORT_PAGES: usize = 10_000; const MAX_IMPORT_BYTES: u64 = 512 * 1024 * 1024; const MAX_IMPORT_REFERENCES: u64 = 20_000;` (attachment + board references; zip entries looked up by exact name with `index_for_name_raw`)
- Before the first create: pages = `manifest.docs.len()`; bytes = Σ over every attachment and board reference of `min(zip entry declared size, MAX_ENTRY_BYTES)` (a missing entry counts 0 and is skipped later as today). Over → `413 import.too_large` (`json_err_with_details` with `{pages, bytes, max_pages, max_bytes, references, max_references}`).
- Reading: `take(declared_size + 1)`; more than declared → skip like other unreadable entries.
- Expose the planned totals (`ImportPlan { pages, blob_bytes }`) for Task 6's quota check.

- [ ] Failing tests: too many pages, too many bytes via one entry referenced many times, entry larger than declared skipped, a normal export/import round-trip unchanged.
- [ ] Implement; tests; clippy; commit `fix(import): cap the pages and bytes one import may write (#88)`.

### Task 6: Quotas

**Files:** Create `migrations/20261112120000_quota_indexes.sql`, `crates/knot-storage/src/quota.rs`, `crates/knot-server/src/quota.rs`; modify `crates/knot-storage/src/lib.rs`, `crates/knot-storage/src/audit.rs` (`record_with_data(pool, workspace_id, actor, action, target_kind, target_id, data: &JsonValue)`, best effort like `record`), `routes/api/docs.rs` (create, from-template, duplicate pre-check), `crates/knot-server/src/duplicate.rs` (`DuplicateError::QuotaExceeded(QuotaRefusal)` after the page count and blob bytes are known, before `record_pending_copies`; audit in `run`), `routes/api/export_import.rs` (after the Task 5 plan), `routes/api/blobs.rs` (before `blobs.insert`); tests `crates/knot-storage/tests/quota.rs`, `crates/knot-server/tests/quota_integration.rs`.

- Migration: `SET LOCAL lock_timeout = '5s'; SELECT knot_lock_tables('quota_indexes', ARRAY['documents','blobs']::regclass[], 'SHARE');` then `CREATE INDEX IF NOT EXISTS documents_created_by_idx ON documents (workspace_id, created_by);` and `CREATE INDEX IF NOT EXISTS blobs_created_by_bytes_idx ON blobs (workspace_id, created_by) INCLUDE (byte_size);`.
- Storage: `pub async fn pages_in_workspace(pool, ws) -> sqlx::Result<u64>`, `pages_created_by(pool, ws, user)`, `blob_bytes_in_workspace(pool, ws)`, `blob_bytes_created_by(pool, ws, user)` — all rows, archived included.
- Server: `pub struct QuotaRefusal { pub quota: &'static str /* "pages"|"blob_bytes" */, pub scope: &'static str /* "user"|"workspace" */, pub limit: u64, pub used: u64, pub requested: u64 }`; `pub async fn check_pages(state, ctx, requested: u64) -> Result<(), QuotaCheckError>` and `check_blob_bytes(...)` (skip every query when the quota is 0; user scope first); `pub fn refusal_response(&QuotaRefusal) -> Response` (403 `quota.exceeded`, details `{quota, scope, limit}`); `pub async fn record_refusal(state, ctx, &QuotaRefusal)` — metric always, audit at most once per (user, quota, scope) per 10 minutes (in-memory map on `AppState`, pruned like the buckets).

- [ ] Failing tests: storage counts (archived included, other users/workspaces excluded); each site refuses at the boundary with 403 and writes nothing; under the quota it works; quota 0 → no counting (assert via a store that would fail, or by timing-free logic in unit tests); audit once per window.
- [ ] Implement; tests; clippy; commit `feat(server): optional page and file-byte quotas (#88)`.

### Task 7: Web — limit messages

**Files:** Modify `web/src/lib/api.ts` (+ test), `web/src/stores/ui.ts` (`notify(kind, text, opts?: { key?: string; durationMs?: number })`, a toast with the same key is replaced and its timer restarts; `durationMs` replaces the 4 s default, for the upload pause notice) and `web/src/components/Toast.tsx`; create `web/src/lib/limitErrors.ts` (+ test): `limitErrorText(error: { status: number; code: string; details?: Record<string, unknown> }): string | null`; wire the call sites listed in the spec's Client section and the map below; `web/src/features/editor/KnotEditor.tsx` upload loop (`features/editor/uploadFiles.ts`, aborted by an `AbortSignal` when the doc changes or the editor unmounts); `web/src/features/export/ExportMenu.tsx` and `web/src/features/workspace/SettingsPage.tsx` zip exports become fetch → blob → save (reuse the existing save helper in `features/export/`), import parses the envelope; `LoginPage`, `PublicDoc`.

Call sites (show `limitErrorText(err)` with `notify("warn", text, { key: "limit" })` in place of the generic error when it returns text): tree create/template/duplicate/move/rename/archive (`features/docs/DocTree.tsx`, `TreeRow.tsx`, `DuplicateDialog.tsx` `duplicateErrorText`), command-palette create (`components/CommandPalette.tsx`, today silent), the Tasks page toggle (`features/tasks/TasksPage.tsx`, today silent and never rolled back: a refusal now throws so the checkbox goes back), comments and replies/reactions (`features/comments/*`), suggestions, markdown import (`ImportMarkdownButton.tsx`), DOCX/ODT export (`features/export/useExport.ts` `fail` takes the error), history restore (`HistoryDrawer.tsx`), board create (`EditorToolbar.tsx`), grants/shares/members dialogs, workspace import. Background writes stay silent.

Texts: rate limited → `You're doing that too fast. Try again in ${n} s.` (no seconds → `… Try again in a moment.`); quota → `You've reached your page limit.` / `This workspace has reached its page limit.` / `You've reached your file storage limit.` / `This workspace has reached its file storage limit.`

- [ ] Failing vitest: helper cases; toast key replacement; upload loop waits and retries once on 429, stops with one toast on quota; zip export shows the text on 429; at least one test per wired feature area.
- [ ] Implement; `cd web && NODE_OPTIONS=--no-experimental-webstorage npx pnpm vitest run`, `npx pnpm tsc --noEmit` (or the repo's typecheck script), eslint; commit `feat(web): show rate-limit and quota messages (#88)`.

### Task 8: Operator surface

**Files:** `deploy/helm/knot/values.yaml` (`rateLimit: {enabled: true, scale: 1, collabMaxSockets: 64}`, `trustedProxies: []`, `quotas: {pagesPerUser: 0, pagesPerWorkspace: 0, blobBytesPerUser: 0, blobBytesPerWorkspace: 0}`, `alerting.rateLimitedPerSecond`, the `replicaCount` comment), `values.schema.json`, `templates/configmap.yaml` (every number `| int64 | quote` except `scale` which is `| toString | quote`; `trustedProxies` via `{{- with }}` and `join ","` accepting a list or a string), `templates/prometheusrule.yaml` (group `knot.limits`, alert `KnotRateLimitedSustained`), `deploy/helm/knot/README.md` (section "Rate limits and quotas", upgrade note in the unreleased upgrade section, Replicas and Alerts bullets, trusted-proxy guidance with networkPolicy, the remaining fill rates), `deploy/grafana/knot.json` + `deploy/grafana/README.md` (row "Limits"), `docs/SLO.md`, `SECURITY.md`, `.env.example`, a chart render check script or Makefile target used in CI-equivalent locally (`helm template` defaults and a 10 GiB quota, assert no `e+`; values the schema must refuse: `/0` and zero-padded `/00` proxies, non-IP characters, a quota past 2^53 − 1, a `--set` string scale).

- [ ] Render with `helm template deploy/helm/knot -f deploy/helm/knot/ci/ct-values.yaml` (and with `--set-json` lists) and check the ConfigMap; `helm lint`; commit `feat(helm): rate-limit, trusted-proxy and quota values (#88)`.

### Task 9: Docs, changelog, e2e

- [x] `CHANGELOG.md` `## [Unreleased]`: Upgrading (429s on writes, set `trustedProxies` behind an ingress), Added (limits, quotas, metrics, chart values), Changed/Fixed (import caps; login throttle behind a trusted proxy) — each ending `(#88)`.
- [x] Outcome doc `docs/superpowers/research/2026-11-12-rate-limits-outcome.md`; row in `docs/superpowers/README.md`.
- [ ] Full suite: `cargo test --workspace`, clippy, fmt, vitest, tsc, eslint; e2e with Docker per the repo's e2e setup on the default limits. Not done yet: Docker and Postgres were unavailable, so the DB-backed tests and the e2e suite have only been compiled, not run (see the outcome doc's Gates).
