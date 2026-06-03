# Observability Implementation Plan (Plan 10)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn knot's stubbed observability surface (Plan 2 wired logging + an empty Prometheus exporter + an OTLP-init code path with no callers) into something an operator can actually use: real HTTP / CRDT / database metrics, tracing spans on hot paths, a Helm-installable ServiceMonitor, an OTLP exporter switch in the chart, a Grafana dashboard, and documented SLOs.

**Architecture:**
- **Metrics** — use the existing `metrics` crate facade (`counter!`, `histogram!`, `gauge!`) that the `metrics-exporter-prometheus` recorder already serves. Three families: `knot_http_*` (request/response on the axum layer), `knot_room_*` (CRDT room actor counters + gauges), and `knot_db_*` (pool-level via a sqlx event hook). Stable label sets only — never per-doc-id or per-user — so cardinality stays bounded.
- **Tracing** — add `tower-http::trace::TraceLayer` on the axum router and `#[tracing::instrument]` on the room actor and high-traffic store methods. OTLP is enabled by `KNOT_TRACING_ENABLED=true` + `KNOT_OTLP_ENDPOINT=...` (already plumbed in `main.rs`). For local dev, `KNOT_LOG_FORMAT=pretty` makes spans visible in the console.
- **Chart** — new `metrics.enabled` (default true) toggles a `:9090` containerPort + a sidecar-free `metrics`-named Service port. A new `serviceMonitor.enabled` (default false) renders a `monitoring.coreos.com/v1 ServiceMonitor` for kube-prometheus-stack users. OTLP is wired via existing env vars in `values.yaml`.
- **Dashboard** — a single Grafana dashboard JSON under `deploy/grafana/knot.json`. Imports cleanly into any Grafana 9+ with a Prometheus datasource.
- **SLOs** — short markdown doc at `docs/SLO.md`: latency targets per endpoint class, error budget, runbook pointers.

**Tech Stack:** `metrics` 0.24 + `metrics-exporter-prometheus` 0.16 (both already in tree), `tower-http` `trace` feature, `tracing` + `tracing-subscriber` (already there). No new crate dependencies for emitting metrics or spans — Plan 10 is pure plumbing on existing deps.

**Predecessor:** Plan 9 (deployment, outcome at `docs/superpowers/research/2026-06-03-plan9-outcome.md`, HEAD `0c5f280`). The Helm chart now exists; Plan 10 amends it with metrics surface + optional ServiceMonitor.

**Spec coverage:**

| Spec section | Tasks |
|---|---|
| §12.1 HTTP request metrics — RED method | T1, T2 |
| §12.2 CRDT room metrics — active rooms, updates, snapshots, room latency | T3 |
| §12.3 DB pool metrics + slow-query histogram | T4 |
| §12.4 Tracing spans on hot paths | T5, T6 |
| §12.5 ServiceMonitor + OTLP-in-chart | T7, T8 |
| §12.6 Sample Grafana dashboard | T9 |
| §12.7 SLO documentation | T10 |

**Out of scope** (intentionally deferred):

- **`/metrics` authn/authz** — the chart exposes the metrics port on a cluster-internal Service. A NetworkPolicy template + bearer-token check are hardening, not v0.1.
- **Trace-based SLOs / exemplars** — wiring Prometheus exemplar IDs through trace IDs is nice but adds a feature flag. Plan after this one.
- **PodMonitor + DaemonSet log shipping** — Loki / Vector wiring is a separate ops choice.
- **OpenTelemetry-format metrics** — staying on Prometheus-pull avoids the OTel collector dependency. Migration is a separate plan.
- **Alerting rules (PrometheusRule)** — the dashboard surfaces the signals; on-call alert rules are org-specific. SLO doc lists what to alert on; a follow-up adds a `PrometheusRule` template.
- **Synthetic probes / blackbox monitoring** — separate plan.

---

## File map

```
crates/knot-server/
├── Cargo.toml                                  (modify) +metrics, +tower-http trace feature
├── src/lib.rs                                  (modify) TraceLayer + metrics middleware
├── src/metrics.rs                              (new) axum middleware: knot_http_* counters
└── tests/metrics_integration.rs                (new) hits a few routes + asserts /metrics output

crates/knot-crdt/
├── Cargo.toml                                  (modify) +metrics
└── src/rooms.rs                                (modify) emit knot_room_* + #[instrument]
                                                          (or wherever the registry/actor lives —
                                                           grep first)

crates/knot-storage/
├── Cargo.toml                                  (modify) +metrics
└── src/pool.rs                                 (modify) PgPool wrapper hook: knot_db_pool_*

crates/knot-obs/
└── src/metrics.rs                              (modify) describe well-known metric names
                                                          so they appear with 0 samples

deploy/helm/knot/
├── values.yaml                                 (modify) +metrics.enabled, +serviceMonitor.*,
                                                          +otlp.* shortcuts (these already
                                                          pass through via the existing
                                                          KNOT_TRACING_ENABLED env)
├── values.schema.json                          (modify) new keys
├── templates/deployment.yaml                   (modify) metrics containerPort + env
├── templates/service.yaml                      (modify) metrics named port
└── templates/servicemonitor.yaml               (new) CRD-gated

deploy/grafana/
└── knot.json                                   (new) sample dashboard

docs/
└── SLO.md                                      (new) latency targets + error budget
```

---

## Conventions

- **Metric naming** — `knot_<subsystem>_<name>_<unit>`. Counters end in `_total`. Histograms end in `_seconds` for latency or `_bytes` for size. No camelCase. Label keys are snake_case.
- **Label cardinality** — bounded labels only. ✓ method + route template + status_class. ✗ exact path, doc id, user id. The axum middleware uses `MatchedPath` to get the route template, not the raw URI.
- **Span naming** — `subsystem.operation` (e.g., `room.apply_update`, `db.users.find_by_email`). Fields use snake_case and never include secrets or PII.
- **Instrumentation surface** — pick the smallest set of spans that gives you a useful trace tree: HTTP request → handler → store call. Don't `#[instrument]` everything — that's noise.
- **TraceLayer placement** — outermost middleware so it sees the full request lifetime including auth/CSRF middleware time.

---

## Task overview

| # | Title | LOC ≈ |
|---|---|---|
| 1 | knot-obs: describe well-known metric names | 80 |
| 2 | knot-server: axum HTTP metrics middleware | 160 |
| 3 | knot-crdt: room actor metrics (active, updates, snapshots) + tracing spans | 130 |
| 4 | knot-storage: pool gauges + slow-query histogram | 110 |
| 5 | knot-server: TraceLayer + per-route #[instrument] on auth/docs/collab | 120 |
| 6 | Integration test: hit /healthz + /api/docs, assert /metrics output | 180 |
| 7 | Helm: metrics containerPort + Service port + values keys | 90 |
| 8 | Helm: ServiceMonitor template + values.schema.json keys | 110 |
| 9 | Sample Grafana dashboard (JSON) | 200 |
| 10 | docs/SLO.md | 0 |
| 11 | Helm: optional OTLP env shortcuts (default off) | 50 |
| 12 | Outcome doc + tag | 0 |

---

## Task 1: knot-obs — describe well-known metric names

**Files:**
- Modify: `crates/knot-obs/src/metrics.rs`

The `metrics` facade lets us `describe_counter!`/`describe_histogram!` upfront so the names appear in `/metrics` output with zero samples — Grafana panels don't have to wait for first traffic to render.

- [ ] **Step 1: Add describe calls**

Edit `init()` in `crates/knot-obs/src/metrics.rs`. AFTER the `.install()` call, add:

```rust
use metrics::{describe_counter, describe_histogram, describe_gauge, Unit};

// HTTP layer
describe_counter!(
    "knot_http_requests_total",
    "HTTP requests handled, labeled by method, route, status_class"
);
describe_histogram!(
    "knot_http_request_duration_seconds",
    Unit::Seconds,
    "HTTP request duration"
);

// CRDT rooms
describe_gauge!(
    "knot_room_active",
    "Currently loaded rooms in this process"
);
describe_counter!(
    "knot_room_updates_total",
    "CRDT updates applied to rooms, by source (local|peer)"
);
describe_counter!(
    "knot_room_snapshots_total",
    "Snapshots written to storage"
);

// Storage / pool
describe_gauge!(
    "knot_db_pool_size",
    "Total connections in the pool"
);
describe_gauge!(
    "knot_db_pool_idle",
    "Idle connections in the pool"
);
describe_histogram!(
    "knot_db_query_duration_seconds",
    Unit::Seconds,
    "Database query duration, by store method"
);
```

- [ ] **Step 2: Update Cargo.toml**

Verify `metrics` is already a dep of `knot-obs`. If not (it should be — `metrics-exporter-prometheus` pulls it transitively, but `describe_*` macros need a direct dep):

```toml
metrics = "0.24"
```

- [ ] **Step 3: Verify**

```bash
cargo check -p knot-obs
cargo test -p knot-obs
```

- [ ] **Step 4: Commit**

```bash
git add crates/knot-obs/
git commit -m "feat(knot-obs): describe well-known metric names"
```

---

## Task 2: HTTP metrics middleware

**Files:**
- Modify: `crates/knot-server/Cargo.toml` — add `metrics`
- Create: `crates/knot-server/src/metrics.rs`
- Modify: `crates/knot-server/src/lib.rs` — mount the middleware on the router

- [ ] **Step 1: Find the router-builder**

```bash
grep -n "Router::new\|fn router\|fn router_with_state" crates/knot-server/src/lib.rs | head -5
```

Note the name (Plan 6 used `router_with_state`; T2 of Plan 9 added a `.fallback_service` here).

- [ ] **Step 2: Write the middleware**

Create `crates/knot-server/src/metrics.rs`:

```rust
//! HTTP request/response metrics middleware.
//!
//! Emits two metrics with bounded labels:
//!   knot_http_requests_total{method,route,status_class}
//!   knot_http_request_duration_seconds{method,route,status_class}
//!
//! `route` is the AxumPath template (e.g. "/api/docs/:id"), NOT the
//! raw URI — keeps cardinality bounded.

use std::time::Instant;

use axum::{
    body::Body,
    extract::MatchedPath,
    http::{Request, Response},
    middleware::Next,
};
use metrics::{counter, histogram};

pub async fn record(req: Request<Body>, next: Next) -> Response<Body> {
    let method = req.method().clone();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());

    let start = Instant::now();
    let resp = next.run(req).await;
    let elapsed = start.elapsed().as_secs_f64();

    let status = resp.status().as_u16();
    let status_class = match status / 100 {
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        5 => "5xx",
        _ => "other",
    };

    let labels = [
        ("method", method.to_string()),
        ("route", route),
        ("status_class", status_class.to_string()),
    ];
    counter!("knot_http_requests_total", &labels).increment(1);
    histogram!("knot_http_request_duration_seconds", &labels).record(elapsed);

    resp
}
```

- [ ] **Step 3: Mount it**

In `crates/knot-server/src/lib.rs` (the router builder), add:

```rust
use axum::middleware;
// ...
.layer(middleware::from_fn(crate::metrics::record))
```

at the OUTERMOST layer (so timings include everything inside, including auth and CSRF middleware).

- [ ] **Step 4: Cargo.toml**

Add to `[dependencies]`:

```toml
metrics = "0.24"
```

(Already in `knot-obs`; per-crate use needs its own dep.)

- [ ] **Step 5: Verify**

```bash
cargo check -p knot-server
cargo test -p knot-server
cargo clippy -p knot-server --all-targets --all-features -- -D warnings
```

- [ ] **Step 6: Commit**

```bash
git add crates/knot-server/
git commit -m "feat(knot-server): HTTP request metrics middleware"
```

---

## Task 3: CRDT room metrics + spans

**Files:**
- Modify: `crates/knot-crdt/Cargo.toml` — add `metrics`
- Modify: the room actor source

- [ ] **Step 1: Find the room actor file**

```bash
grep -rln "spawn_actor\|RoomActor\|active.*room\|tokio::select" crates/knot-crdt/src/
```

Probably `room.rs` or `rooms.rs` (registry + actor).

- [ ] **Step 2: Emit metrics**

In the room registry (the `Rooms` struct), wrap inserts/removes:

```rust
use metrics::{counter, gauge};

// after a room is inserted into the registry:
gauge!("knot_room_active").increment(1.0);
// after a room is evicted:
gauge!("knot_room_active").decrement(1.0);
```

In the actor's update path (wherever `Y.apply_update` happens):

```rust
counter!("knot_room_updates_total", &[("source", source_str)]).increment(1);
```

where `source_str` is `"local"` for HTTP-originated, `"peer"` for LISTEN/NOTIFY-replayed updates. Check Plan 5 for the exact names.

In the snapshot writer task:

```rust
counter!("knot_room_snapshots_total").increment(1);
```

- [ ] **Step 3: Add spans**

On the actor's `apply_update` and `snapshot` methods, add:

```rust
#[tracing::instrument(skip(self, update), fields(doc_id = %self.doc_id, bytes = update.len()))]
async fn apply_update(...) { ... }
```

(Skip `update` — it's bytes; emit `bytes = update.len()` instead.)

- [ ] **Step 4: Cargo.toml**

Add `metrics = "0.24"` to `[dependencies]`. `tracing` is already there.

- [ ] **Step 5: Verify**

```bash
cargo check -p knot-crdt
cargo test -p knot-crdt
cargo clippy -p knot-crdt --all-targets --all-features -- -D warnings
```

- [ ] **Step 6: Commit**

```bash
git add crates/knot-crdt/
git commit -m "feat(knot-crdt): room metrics + tracing spans"
```

---

## Task 4: DB pool metrics + slow-query histogram

**Files:**
- Modify: `crates/knot-storage/Cargo.toml`
- Modify: `crates/knot-storage/src/pool.rs`

- [ ] **Step 1: Periodic gauge poll**

`sqlx::PgPool` exposes `size()` and `num_idle()`. Spawn a background task in `connect()` that wakes every 10 s and updates the gauges:

```rust
use std::time::Duration;
use metrics::gauge;

// in connect(), AFTER migrations run:
let p = pool.clone();
tokio::spawn(async move {
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tick.tick().await;
        gauge!("knot_db_pool_size").set(p.size() as f64);
        gauge!("knot_db_pool_idle").set(p.num_idle() as f64);
    }
});
```

- [ ] **Step 2: Slow-query histogram (defer)**

A real per-query histogram needs `sqlx::Executor` to be wrapped. That's invasive — skip in this task and capture as a deferred item in the outcome doc. The pool gauges + the HTTP-level latency histogram give 80% of the value for 20% of the work.

- [ ] **Step 3: Cargo.toml**

Add:

```toml
metrics = "0.24"
```

- [ ] **Step 4: Verify**

```bash
cargo check -p knot-storage
cargo test -p knot-storage
```

- [ ] **Step 5: Commit**

```bash
git add crates/knot-storage/
git commit -m "feat(knot-storage): pool size + idle gauges"
```

---

## Task 5: TraceLayer + #[instrument] on hot paths

**Files:**
- Modify: `crates/knot-server/Cargo.toml` — `tower-http` `trace` feature
- Modify: `crates/knot-server/src/lib.rs` — TraceLayer
- Modify: a few handlers — `#[instrument]`

- [ ] **Step 1: tower-http feature**

```bash
grep tower-http crates/knot-server/Cargo.toml
```

Add `"trace"` to the existing feature list.

- [ ] **Step 2: Mount TraceLayer**

In the router builder, BEFORE the metrics middleware (so spans wrap metrics):

```rust
use tower_http::trace::TraceLayer;
// ...
.layer(TraceLayer::new_for_http())
```

- [ ] **Step 3: Instrument handlers**

Add `#[tracing::instrument(skip(...))]` on:

- `crates/knot-server/src/routes/auth/local.rs::login` — skip Json body fields (PII)
- `crates/knot-server/src/routes/auth/local.rs::change_password` — skip body
- `crates/knot-server/src/routes/api/docs.rs::create` / `rename` / `archive`
- `crates/knot-server/src/lib.rs::collab_upgrade`

Don't `#[instrument]` everything — these are the most useful handles in a trace.

- [ ] **Step 4: Verify**

```bash
cargo check -p knot-server
cargo test -p knot-server
cargo clippy -p knot-server --all-targets --all-features -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add crates/knot-server/
git commit -m "feat(knot-server): TraceLayer + instrument hot path handlers"
```

---

## Task 6: Integration test — /metrics surface

**Files:**
- Create: `crates/knot-server/tests/metrics_integration.rs`

- [ ] **Step 1: Write the test**

```rust
//! Hits a couple of routes, then scrapes /metrics and asserts the
//! expected metric families appear with at least one sample.
//!
//! NB: `/metrics` is served on a SEPARATE port (KNOT_METRICS_ADDR /
//! default :9090) — it is NOT on the main axum router. So this test
//! has to install the recorder + start the exporter explicitly, then
//! tower-call the app + scrape via reqwest.

use axum::body::Body;
use http::{Request, StatusCode};
use knot_test_support::fresh_db;
use tower::ServiceExt;

#[tokio::test(flavor = "multi_thread")]
async fn metrics_endpoint_lists_described_names_after_traffic() {
    let db = fresh_db().await;
    // Install the exporter on a random port.
    let port = pick_free_port();
    let addr = format!("127.0.0.1:{port}");
    knot_obs::metrics::init(&addr).expect("install exporter");

    let mut state = knot_server::AppState::with_pool(db.pool.clone());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    let app = knot_server::router_with_state(state);

    // Generate traffic on a couple of routes.
    for _ in 0..3 {
        let r = app.clone().oneshot(
            Request::builder().method("GET").uri("/api/healthz")
                .body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    // Give the recorder a beat.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let body = reqwest::get(format!("http://{addr}/metrics"))
        .await.unwrap()
        .text().await.unwrap();

    assert!(body.contains("knot_http_requests_total"));
    assert!(body.contains("knot_http_request_duration_seconds"));
    // Just the existence of the gauge family is enough — the actor metrics
    // are zero-sample if no rooms have been created.
    assert!(body.contains("knot_room_active"));
}

fn pick_free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}
```

> **Caveat:** the `metrics` global recorder can only be installed once per process. `cargo nextest` runs each test in a separate process so this test is isolated. With `cargo test --jobs 1` the same applies. Within a single process you'd need to use a `Once` guard.

- [ ] **Step 2: Add reqwest dev-dep if missing**

```bash
grep reqwest crates/knot-server/Cargo.toml
```

- [ ] **Step 3: Run**

```bash
cargo nextest run -p knot-server --test metrics_integration
```

- [ ] **Step 4: Commit**

```bash
git add crates/knot-server/
git commit -m "test(knot-server): /metrics integration"
```

---

## Task 7: Helm — metrics surface

**Files:**
- Modify: `deploy/helm/knot/values.yaml`
- Modify: `deploy/helm/knot/values.schema.json`
- Modify: `deploy/helm/knot/templates/deployment.yaml`
- Modify: `deploy/helm/knot/templates/service.yaml`

- [ ] **Step 1: values.yaml**

Add a new top-level block:

```yaml
metrics:
  enabled: true
  port: 9090
```

- [ ] **Step 2: deployment.yaml**

Add a second containerPort + env var:

```yaml
ports:
  - name: http
    containerPort: 3000
  {{- if .Values.metrics.enabled }}
  - name: metrics
    containerPort: {{ .Values.metrics.port }}
  {{- end }}
env:
  {{- if .Values.metrics.enabled }}
  - name: KNOT_METRICS_ADDR
    value: ":{{ .Values.metrics.port }}"
  {{- end }}
```

(Append the `env:` block to the existing container spec.)

- [ ] **Step 3: service.yaml**

Add a metrics port:

```yaml
ports:
  - name: http
    port: {{ .Values.service.port }}
    targetPort: 3000
  {{- if .Values.metrics.enabled }}
  - name: metrics
    port: {{ .Values.metrics.port }}
    targetPort: metrics
  {{- end }}
```

- [ ] **Step 4: values.schema.json**

Add:

```json
"metrics": {
  "type": "object",
  "properties": {
    "enabled": { "type": "boolean" },
    "port": { "type": "integer", "minimum": 1, "maximum": 65535 }
  }
}
```

- [ ] **Step 5: Verify**

```bash
helm lint deploy/helm/knot --set database.url=x --set session.key=y
helm template knot deploy/helm/knot --set database.url=x --set session.key=y | grep -A 2 metrics
```

- [ ] **Step 6: Commit**

```bash
git add deploy/helm/
git commit -m "feat(deploy): metrics containerPort + Service port"
```

---

## Task 8: Helm — ServiceMonitor

**Files:**
- Create: `deploy/helm/knot/templates/servicemonitor.yaml`
- Modify: `deploy/helm/knot/values.yaml`
- Modify: `deploy/helm/knot/values.schema.json`

- [ ] **Step 1: values.yaml**

```yaml
serviceMonitor:
  enabled: false
  interval: 30s
  scrapeTimeout: 10s
  namespace: ""        # defaults to release namespace
  labels: {}           # extra labels for Prometheus operator matching
  metricRelabelings: []
  relabelings: []
```

- [ ] **Step 2: template**

```yaml
{{- if and .Values.metrics.enabled .Values.serviceMonitor.enabled -}}
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: {{ include "knot.fullname" . }}
  {{- with .Values.serviceMonitor.namespace }}
  namespace: {{ . }}
  {{- end }}
  labels:
    {{- include "knot.labels" . | nindent 4 }}
    {{- with .Values.serviceMonitor.labels }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
spec:
  selector:
    matchLabels:
      {{- include "knot.selectorLabels" . | nindent 6 }}
  endpoints:
    - port: metrics
      path: /metrics
      interval: {{ .Values.serviceMonitor.interval }}
      scrapeTimeout: {{ .Values.serviceMonitor.scrapeTimeout }}
      {{- with .Values.serviceMonitor.relabelings }}
      relabelings:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.serviceMonitor.metricRelabelings }}
      metricRelabelings:
        {{- toYaml . | nindent 8 }}
      {{- end }}
{{- end -}}
```

- [ ] **Step 3: values.schema.json**

```json
"serviceMonitor": {
  "type": "object",
  "properties": {
    "enabled": { "type": "boolean" },
    "interval": { "type": "string" },
    "scrapeTimeout": { "type": "string" },
    "namespace": { "type": "string" },
    "labels": { "type": "object" }
  }
}
```

- [ ] **Step 4: Verify**

```bash
helm lint deploy/helm/knot --set database.url=x --set session.key=y --set serviceMonitor.enabled=true
helm template knot deploy/helm/knot --set database.url=x --set session.key=y --set serviceMonitor.enabled=true | grep -A 5 ServiceMonitor
```

- [ ] **Step 5: Commit**

```bash
git add deploy/helm/
git commit -m "feat(deploy): optional ServiceMonitor for kube-prometheus-stack"
```

---

## Task 9: Sample Grafana dashboard

**Files:**
- Create: `deploy/grafana/knot.json`
- Create: `deploy/grafana/README.md`

- [ ] **Step 1: Build a dashboard**

A minimal but useful dashboard, three rows:

1. **Top-line** — Requests/sec, P95 latency, 5xx rate, active rooms (4 stat panels)
2. **HTTP** — Requests by route (timeseries), latency P50/P95/P99 (timeseries), error rate by route (timeseries)
3. **Internals** — Active rooms (gauge), CRDT updates/s by source (timeseries), DB pool size & idle (timeseries), snapshots/s (timeseries)

Use only the metric names from Task 1. PromQL snippets to embed:

```promql
# Requests/sec
sum(rate(knot_http_requests_total[5m]))

# P95 latency overall
histogram_quantile(0.95, sum(rate(knot_http_request_duration_seconds_bucket[5m])) by (le))

# Error rate
sum(rate(knot_http_requests_total{status_class=~"5xx"}[5m])) / sum(rate(knot_http_requests_total[5m]))

# Active rooms
knot_room_active

# Updates/sec by source
sum(rate(knot_room_updates_total[1m])) by (source)

# Pool occupancy
knot_db_pool_size - knot_db_pool_idle
```

Use Grafana 9+ schema. Set `schemaVersion: 39`. Build via the Grafana UI then export the JSON, or hand-write — either works. Aim for ~200 lines of JSON.

- [ ] **Step 2: README**

`deploy/grafana/README.md`:

```markdown
# Grafana dashboard

Import `knot.json` into Grafana 9+ with a Prometheus datasource:

    Dashboards → Import → Upload JSON file → Pick datasource

The dashboard expects the Prometheus exposed by knot's `/metrics`
endpoint (see the Helm chart `metrics.enabled`).
```

- [ ] **Step 3: Commit**

```bash
git add deploy/grafana/
git commit -m "feat(deploy): sample Grafana dashboard"
```

---

## Task 10: SLO doc

**Files:**
- Create: `docs/SLO.md`

- [ ] **Step 1: Write the doc**

Cover:

- **Availability** — 99.5% on `/api/*` (excluding `/api/healthz`). Measured over 30-day rolling window.
- **Latency** — P95 < 250 ms on `/api/docs`, `/api/workspace/*`. P95 < 500 ms on `/auth/login`. P95 < 2 s on `/auth/setup`.
- **CRDT convergence** — peer update visible on a second client within 1 s P95.
- **Error budget** — calculated per quarter; what spending the budget allows / forbids.
- **Burn-rate alerts** (descriptions only, not PrometheusRule).
- **Pointers** — where to find dashboards, runbooks, and incident retrospectives.

- [ ] **Step 2: Commit**

```bash
git add docs/
git commit -m "docs: initial SLO targets + error-budget framework"
```

---

## Task 11: Helm — OTLP env shortcuts

**Files:**
- Modify: `deploy/helm/knot/values.yaml`
- Modify: `deploy/helm/knot/templates/configmap.yaml`

- [ ] **Step 1: values.yaml**

```yaml
tracing:
  enabled: false
  otlpEndpoint: ""     # e.g. http://otel-collector.observability.svc.cluster.local:4317
```

- [ ] **Step 2: configmap.yaml**

Append:

```yaml
  {{- if .Values.tracing.enabled }}
  KNOT_TRACING_ENABLED: "true"
  KNOT_OTLP_ENDPOINT: {{ required "tracing.otlpEndpoint is required when tracing.enabled=true" .Values.tracing.otlpEndpoint | quote }}
  {{- end }}
```

- [ ] **Step 3: Schema**

```json
"tracing": {
  "type": "object",
  "properties": {
    "enabled": { "type": "boolean" },
    "otlpEndpoint": { "type": "string" }
  }
}
```

- [ ] **Step 4: Verify**

```bash
helm lint deploy/helm/knot --set database.url=x --set session.key=y --set tracing.enabled=true --set tracing.otlpEndpoint=http://otel:4317
```

- [ ] **Step 5: Commit**

```bash
git add deploy/helm/
git commit -m "feat(deploy): tracing.enabled + OTLP endpoint values"
```

---

## Task 12: Outcome doc

**Files:**
- Create: `docs/superpowers/research/2026-06-0X-plan10-outcome.md`

Use the same template as Plan 9. List commits, gates, what was non-obvious, what's deferred, carryforward.

```bash
git add docs/
git commit -m "docs: Plan 10 outcome"
```

---

## Self-review checklist

- [ ] `cargo test --workspace` green (including the new metrics integration)
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean
- [ ] `pnpm tsc/lint/test/playwright` green (no frontend changes — should be untouched)
- [ ] `helm lint` clean across all the new combos
- [ ] `helm template ... | kubectl apply --dry-run=client -f -` accepts the new resources
- [ ] Manual: `make image.build.host && make image.smoke` and `curl http://localhost:9090/metrics` shows `knot_http_requests_total`
- [ ] Manual: send a few requests, observe a useful trace in `KNOT_LOG_FORMAT=pretty` mode
- [ ] Manual: import `deploy/grafana/knot.json` into a local Grafana and check the top-line row renders
