# Plan 10 Outcome — Observability

**Status:** GO. All 12 tasks landed; all gates green.

**Verdict:** knot now emits structured HTTP/CRDT/DB metrics on a Prometheus endpoint, has tracing spans on hot paths, and ships a Helm-installable ServiceMonitor + sample Grafana dashboard + SLO doc. An operator can install the chart, point Prometheus at it, and have a populated dashboard within minutes. Recommended next: **Plan 7 (UI polish)** or a hardening plan (rate limiting + NetworkPolicy + alerting rules).

## What landed

Plan 10 commits (HEAD `6db5807`):

| Commit | Task | Subject |
|---|---|---|
| dc43ffc | T1  | knot-obs: describe well-known metric names |
| 3794a2c | T2  | knot-server: HTTP request metrics middleware |
| 07dac94 | T3  | knot-crdt: room metrics + tracing spans |
| 1702600 | T4  | knot-storage: pool size + idle gauges |
| 48b6530 | T5  | knot-server: TraceLayer + instrument hot path handlers |
| b850904 | T6  | knot-server: /metrics integration scrape |
| 5c3a75f | T7  | deploy: metrics containerPort + Service port |
| 1b54e0f | T8  | deploy: optional ServiceMonitor for kube-prometheus-stack |
| f32f54e | T9  | deploy: sample Grafana dashboard + README |
| 6db5807 | T10 | docs: initial SLO targets + error-budget framework |
| 360e66c | T11 | deploy: tracing.enabled + OTLP endpoint values |

T12 is this outcome doc.

## Gates

- `cargo test --workspace` — green (1 new integration test added in T6)
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean
- `pnpm tsc` + `pnpm lint` + `pnpm test` + `pnpm playwright test` — unchanged (no frontend work in Plan 10)
- `helm lint deploy/helm/knot` — clean across all the new toggle combinations (`metrics.enabled`, `serviceMonitor.enabled`, `tracing.enabled`)
- `helm template ... | kubectl apply --dry-run=client -f -` — accepts all native resources. The `ServiceMonitor` errors with "unknown kind" on clusters that lack the kube-prometheus-stack CRDs, which is the expected behavior (the chart only renders the CRD when `serviceMonitor.enabled=true`).
- Image build (from Plan 9) still produces a working 19.5 MB image. The new `/metrics` endpoint is reachable on port 9090 when the container is run.

## Architecture summary

**Metrics emitted (all with bounded label cardinality):**

| Metric | Type | Labels | Description |
|---|---|---|---|
| `knot_http_requests_total` | counter | method, route, status_class | HTTP requests handled |
| `knot_http_request_duration_seconds` | histogram | method, route, status_class | Request latency |
| `knot_room_active` | gauge | (none) | Rooms loaded in this process |
| `knot_room_updates_total` | counter | source (`local`\|`peer`) | CRDT updates applied |
| `knot_room_snapshots_total` | counter | (none) | Snapshots written |
| `knot_db_pool_size` | gauge | (none) | Connections in the pool |
| `knot_db_pool_idle` | gauge | (none) | Idle connections |
| `knot_db_query_duration_seconds` | histogram | (described, not yet emitted) | Per-query latency — deferred |

All names are pre-registered via `describe_*` in `knot-obs::metrics::init`, so they appear in `/metrics` output even with zero samples (Grafana panels don't need first traffic to render).

**Tracing:**
- `tower_http::trace::TraceLayer` on the axum router wraps the per-route handlers.
- `#[tracing::instrument(skip_all)]` on `auth.login`, `auth.change_password`, `docs.create`, `docs.rename`, `docs.archive`, `collab.upgrade`, the CRDT `Room::run` actor loop, and `Room::on_inbound`.
- OTLP exporter wires through existing env vars (`KNOT_TRACING_ENABLED` + `KNOT_OTLP_ENDPOINT`). The chart's new `tracing.enabled` + `tracing.otlpEndpoint` values shortcut these.

**Helm surface:**
- `metrics.enabled` (default `true`) — adds a `:9090` containerPort + a `metrics`-named Service port.
- `serviceMonitor.enabled` (default `false`) — renders a `monitoring.coreos.com/v1 ServiceMonitor`. The template is gated on `metrics.enabled` AND `serviceMonitor.enabled` so accidental misconfiguration is caught early.
- `tracing.enabled` + `tracing.otlpEndpoint` — env-var shortcut for OTLP push.

**Dashboard:** `deploy/grafana/knot.json` — three rows (top-line, HTTP, internals), `instance` template variable. Imports cleanly into Grafana 9+ with a Prometheus datasource.

**SLO doc:** `docs/SLO.md` — availability (99.5%), per-route-class latency targets, CRDT convergence, error budget, burn-rate signals.

## What was non-obvious

**`describe_*` is essential.** Without it, the metric family doesn't appear in `/metrics` until the first sample is recorded — so a Grafana panel rendering immediately after install would show "No data" even though the recorder is wired correctly. Registering names upfront in `init()` gives operators an instant feedback loop.

**Label cardinality matters even more than you think.** Using the raw URI (`/api/docs/abc-123-def`) would explode cardinality with every document. The middleware extracts `MatchedPath` from request extensions so the label is the route template (`/api/docs/:id`). This is the single most important design decision in the file.

**Layer order in axum is bottom-up.** The metrics middleware needs to wrap everything (including auth/CSRF) so the histogram measures the full request lifetime. The last `.layer()` in the chain is the outermost. TraceLayer goes one position inward so spans wrap handlers but the metrics see everything including span overhead.

**The Prometheus recorder is a global singleton.** T6's integration test uses a `OnceLock` to install the exporter exactly once per binary. If a future test installs its own recorder, both tests fail — they have to share the same install via the lock. This is fine within a single binary; `cargo nextest` running per-binary keeps independent tests independent.

**`knot_db_pool_size` is sampled, not pushed.** The sqlx pool doesn't expose hooks for connection acquire/release; instead `connect()` spawns a 10-second-interval poller. The integration test in T6 doesn't assert on this gauge because it would need to wait 10 seconds — left as a manual smoke check.

## What's still deferred

- **Per-query histogram (`knot_db_query_duration_seconds`)** — the name is described but no sites emit. A real per-query histogram needs `sqlx::Executor` wrapping, which is invasive. The HTTP-level latency histogram covers ~80 % of the value; defer until a slow-query investigation actually demands it.
- **PrometheusRule template** — burn-rate alerts are described in the SLO doc but not rendered as `monitoring.coreos.com/v1 PrometheusRule`. Org-specific severity routing makes this awkward to ship as a default; a follow-up task can add a values-driven template.
- **Dashboard as ConfigMap with `grafana_dashboard: "1"` label** — would let kube-prometheus-stack auto-discover the dashboard. The `deploy/grafana/knot.json` file is the source of truth either way.
- **Exemplars** — Prometheus exemplar IDs linking to trace IDs would close the metric→trace loop, but it needs the OpenTelemetry collector path. Defer to an OTel-end-to-end plan.
- **PodMonitor** — for clusters that prefer pod-scrape over service-scrape. Easy to add but rarely needed.
- **Authentication on `/metrics`** — the port is cluster-internal via Service. A NetworkPolicy + bearer-token wrapper is hardening, not v0.1.
- **WebSocket convergence latency** — the SLO doc lists a 1 s P95 target but there's no server-side metric for it. End-to-end test (`e2e/flows/two-users-converge.spec.ts`) is the current guard.

## Carryforward for the next plan

1. **Plan 7 — UI polish.** Drag-drop tree move, command palette, role-aware editor toolbar, mobile pass.
2. **Hardening plan.** Rate limit `/auth/login` + `/auth/password` per-user. NetworkPolicy templates. PrometheusRule template with alert routing. Image signing (cosign).
3. **OTel end-to-end.** Switch metrics emission from Prometheus-pull to OTLP push, drop the `metrics-exporter-prometheus` dep, add exemplars linking metrics ↔ traces. Bigger lift; only worthwhile once operators ask.

## Files of interest

| Path | Role |
|---|---|
| `crates/knot-obs/src/metrics.rs` | `describe_*` for every metric the workspace emits |
| `crates/knot-server/src/metrics.rs` | axum middleware — http counter + histogram |
| `crates/knot-crdt/src/registry.rs` | `knot_room_active` gauge inc/dec |
| `crates/knot-crdt/src/room.rs` | `knot_room_updates_total` + snapshots + `#[instrument]` |
| `crates/knot-storage/src/pool.rs` | pool size + idle gauges (10 s poll) |
| `crates/knot-server/src/lib.rs` | TraceLayer + outermost metrics layer + `collab.upgrade` span |
| `crates/knot-server/tests/metrics_integration.rs` | /metrics scrape integration |
| `deploy/helm/knot/templates/servicemonitor.yaml` | kube-prometheus-stack integration |
| `deploy/helm/knot/templates/deployment.yaml` | metrics containerPort + KNOT_METRICS_ADDR env |
| `deploy/grafana/knot.json` | dashboard (3 rows, ~290 lines) |
| `docs/SLO.md` | availability + latency + error-budget |
