# Hardening Branch 1 — HTTP-edge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Close the HTTP-edge DoS surface and the blob stored-XSS path, and fix the release-mode WebSocket panic.

**Architecture:** Add global axum/tower-http layers (body-size limit, request timeout, security-header middleware incl. enforced CSP), harden blob downloads (`nosniff` + `attachment` + SVG coercion), cap WebSocket frame sizes, and fix the varuint length overflow in the wire decoder. Introduce a `cookie_secure` config flag (default true) — the security-header layer is its first consumer (HSTS gating); Branch 2 reuses it for cookies.

**Tech Stack:** Rust — axum 0.7.9, tower-http 0.6, knot-config. Tests: `cargo nextest run -p knot-server` (dev-compose Postgres; never testcontainers); `cd e2e && pnpm playwright test`. `cargo clippy -- -D warnings`.

**Spec:** `docs/superpowers/specs/2026-06-24-security-robustness-hardening-design.md` (Branch 1).

**Preconditions:** dev-compose Postgres healthy.

---

## File Structure
- Modify: `crates/knot-server/src/protocol.rs` — overflow-safe `read_var_bytes`.
- Modify: `crates/knot-server/src/routes/api/blobs.rs` + `routes/public.rs` — download headers.
- Modify: `crates/knot-server/src/lib.rs` — WS frame caps; global body-limit/timeout/security-header layers.
- Create: `crates/knot-server/src/security_headers.rs` — the header middleware.
- Modify: `crates/knot-config/src/lib.rs` — `cookie_secure` flag.
- Modify: `crates/knot-server/src/lib.rs` (AppState) + `main.rs` — `cookie_secure` field wiring.
- Modify: root `Cargo.toml` — tower-http `timeout` feature.
- Modify: `.env.example`, `deploy/compose/dev.yml` — `KNOT_COOKIE_SECURE=false` for dev.
- Test: `crates/knot-server/tests/http_hardening_integration.rs` (new); `e2e` full run.

---

## Task 1: Overflow-safe varuint decoder

**Files:** Modify `crates/knot-server/src/protocol.rs`; test in the same file's `#[cfg(test)] mod tests`.

Context: `read_var_bytes` (`protocol.rs:86`) does `let total = consumed + len as usize;` where `len: u64` comes from `read_var_uint` decoding untrusted client bytes. A huge `len` overflows in release → wraps → the `buf.len() < total` guard passes → `&buf[consumed..total]` panics. `decode()` calls this on inbound WS frames.

- [ ] **Step 1: Write the failing test** — add to the existing `mod tests`:

```rust
#[test]
fn read_var_bytes_rejects_oversize_length_without_panicking() {
    // A varuint encoding a huge length, followed by no payload.
    let mut buf = Vec::new();
    append_var_uint(&mut buf, u64::MAX); // 10 continuation bytes
    // No payload follows; an honest decoder must return Truncated, not panic.
    let r = read_var_bytes(&buf);
    assert!(matches!(r, Err(DecodeError::Truncated)));
}

#[test]
fn read_var_bytes_reads_a_valid_payload() {
    let mut buf = Vec::new();
    append_var_uint(&mut buf, 3);
    buf.extend_from_slice(b"abc");
    let (payload, total) = read_var_bytes(&buf).unwrap();
    assert_eq!(payload, b"abc");
    assert_eq!(total, buf.len());
}
```

Run: `cargo nextest run -p knot-server --lib protocol::` → the oversize test FAILS (panic/overflow) in dev.

- [ ] **Step 2: Implement** — replace the body of `read_var_bytes`:

```rust
fn read_var_bytes(buf: &[u8]) -> Result<(&[u8], usize), DecodeError> {
    let (len, consumed) = read_var_uint(buf)?;
    let len = usize::try_from(len).map_err(|_| DecodeError::Truncated)?;
    let total = consumed.checked_add(len).ok_or(DecodeError::Truncated)?;
    if buf.len() < total {
        return Err(DecodeError::Truncated);
    }
    Ok((&buf[consumed..total], total))
}
```

- [ ] **Step 3: Run** — `cargo nextest run -p knot-server --lib protocol::` → PASS. `cargo clippy -p knot-server --all-targets -- -D warnings` → clean.

- [ ] **Step 4: Commit**
```bash
git add crates/knot-server/src/protocol.rs
git commit -m "fix(protocol): reject oversize varuint length instead of panicking"
```

---

## Task 2: Blob download hardening (nosniff + attachment + SVG coercion)

**Files:** Modify `crates/knot-server/src/routes/api/blobs.rs` (download handler ~`:202`) and `crates/knot-server/src/routes/public.rs` (public blob mirror ~`:243`). Test: new `crates/knot-server/tests/http_hardening_integration.rs`.

Context: the download serves `meta.content_type` (uploader-controlled) inline with no `nosniff`/`Content-Disposition`; the blocklist (`blobs.rs:24`) misses `image/svg+xml`/`text/html` → stored XSS.

- [ ] **Step 1: Add a shared safe-content-type helper** in `blobs.rs` (near the blocklist const):

```rust
/// Content types we are willing to serve INLINE. Everything else is sent as a
/// download with a neutral type so a browser never renders attacker-controlled
/// markup (e.g. an uploaded SVG/HTML) in our origin.
fn safe_inline_content_type(ct: &str) -> Option<&'static str> {
    match ct {
        "image/png" => Some("image/png"),
        "image/jpeg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        "application/pdf" => Some("application/pdf"),
        _ => None,
    }
}
```

- [ ] **Step 2: Use it in the blob download response** — replace the `Response::builder()...` block in the `download_blob` handler (`blobs.rs:~202`):

```rust
    let (ct, disposition) = match safe_inline_content_type(&meta.content_type) {
        Some(ct) => (ct, "inline"),
        None => ("application/octet-stream", "attachment"),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ct)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::CACHE_CONTROL, "private, max-age=60")
        .header(header::CONTENT_LENGTH, meta.byte_size)
        .body(Body::from(bytes))
        .unwrap()
```

Apply the identical change to the public mirror in `routes/public.rs` (the blob-serving response there). If `public.rs` cannot import the helper (private), make `safe_inline_content_type` `pub(crate)` in `blobs.rs` and import it.

- [ ] **Step 3: Write the integration test** — create `crates/knot-server/tests/http_hardening_integration.rs`. Mirror `blobs_integration.rs` for auth/setup + upload; then download and assert headers. (Read `blobs_integration.rs` for the exact login + multipart-upload helper and reuse it verbatim.)

```rust
// After uploading an SVG blob (content-type image/svg+xml) as an editor:
// GET /api/blobs/:id and assert it is NOT served inline as SVG.
let r = /* GET /api/blobs/{id} with sid+csrf */;
assert_eq!(r.status(), StatusCode::OK);
assert_eq!(r.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
assert_eq!(r.headers()[header::CONTENT_TYPE], "application/octet-stream");
assert_eq!(r.headers()[header::CONTENT_DISPOSITION], "attachment");
// And a PNG is still served inline:
// (upload image/png, assert content-type image/png + disposition inline + nosniff)
```

Run: `cargo nextest run -p knot-server --test http_hardening_integration` → PASS.

- [ ] **Step 4: clippy + commit**
```bash
cargo clippy -p knot-server --all-targets -- -D warnings
git add crates/knot-server/src/routes/api/blobs.rs crates/knot-server/src/routes/public.rs crates/knot-server/tests/http_hardening_integration.rs
git commit -m "fix(blobs): serve downloads with nosniff + attachment; coerce non-image types"
```

---

## Task 3: WebSocket frame size cap

**Files:** Modify `crates/knot-server/src/lib.rs` (`collab_upgrade` ~`:204` and `collab_board_upgrade` ~`:251`).

Context: the `WebSocketUpgrade` extractor uses the ~64 MiB default; cap it.

- [ ] **Step 1: Cap both upgrades** — in each handler, before `ws.on_upgrade(...)`, replace `ws` with a size-limited one. Add a const near the top of `lib.rs`:

```rust
/// Max inbound collab/board WS message. CRDT updates and board ops are far
/// smaller; this bounds memory amplification from a malicious frame.
const MAX_WS_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
```

In `collab_upgrade`, change the final block to:
```rust
    let ws = ws.max_message_size(MAX_WS_MESSAGE_BYTES).max_frame_size(MAX_WS_MESSAGE_BYTES);
    ws.on_upgrade(move |socket| async move {
        crate::room::serve(rooms, doc_id, socket, can_write, shutdown).await;
    })
    .into_response()
```
Do the same in `collab_board_upgrade` (board shim).

- [ ] **Step 2: Build + existing tests** — `cargo build -p knot-server && cargo nextest run -p knot-server` → green. (No new test; size enforcement is library behavior. The convergence/board tests confirm normal frames still flow.)

- [ ] **Step 3: Commit**
```bash
git add crates/knot-server/src/lib.rs
git commit -m "fix(collab): cap WebSocket frame size at 4 MiB"
```

---

## Task 4: `cookie_secure` config flag

**Files:** Modify `crates/knot-config/src/lib.rs`, `crates/knot-server/src/lib.rs` (AppState), `crates/knot-server/src/main.rs`, `.env.example`, `deploy/compose/dev.yml`.

Context: the security-header layer (Task 5) needs an "are we secure" signal for HSTS, and Branch 2 reuses it for cookies. `KNOT_*` env names map to `Config` fields; `main.rs:206` shows how config values are copied into `AppState` (e.g. `s.session_key = ...`).

- [ ] **Step 1: Add the config field** — in `Config` (`config/src/lib.rs:57+`), add after `base_url`:
```rust
    /// Set the `Secure` flag on auth cookies and emit HSTS. Default true;
    /// dev sets KNOT_COOKIE_SECURE=false to allow plain-HTTP localhost.
    pub cookie_secure: bool,
```
In the `Default`/loader, default it to `true` (mirror how other bools like `oidc_enabled` get their default; ensure the env parse reads `KNOT_COOKIE_SECURE` as a bool defaulting true). Add a unit test asserting it defaults true and parses `false`.

- [ ] **Step 2: Thread into AppState** — add `pub cookie_secure: bool` to `AppState` (default `true` in its constructors, alongside fields like `base_url`), and in `main.rs` set it from config next to `s.session_key = ...`:
```rust
    s.cookie_secure = cfg.cookie_secure;
```

- [ ] **Step 3: Dev config** — in `.env.example` add `KNOT_COOKIE_SECURE=false` (with a comment: "dev over plain HTTP; MUST be true in production"); in `deploy/compose/dev.yml` add `KNOT_COOKIE_SECURE: "false"` to the knot-server service env if it sets KNOT_* there (if the dev server runs via `make dev` reading `.env`, the `.env.example` entry suffices — check and apply to whichever the dev path uses).

- [ ] **Step 4: Build + config test** — `cargo nextest run -p knot-config && cargo build -p knot-server` → green.

- [ ] **Step 5: Commit**
```bash
git add crates/knot-config/src/lib.rs crates/knot-server/src/lib.rs crates/knot-server/src/main.rs .env.example deploy/compose/dev.yml
git commit -m "feat(config): KNOT_COOKIE_SECURE flag (default true)"
```

---

## Task 5: Global hardening layers (body limit, timeout, security headers + CSP)

**Files:** root `Cargo.toml` (tower-http `timeout` feature); create `crates/knot-server/src/security_headers.rs`; modify `crates/knot-server/src/lib.rs` (router + `mod`). Test: extend `http_hardening_integration.rs`; full e2e run.

- [ ] **Step 1: Enable the timeout feature** — in root `Cargo.toml`, change the tower-http line to add `"timeout"`:
```toml
tower-http = { version = "0.6", features = ["trace", "compression-br", "fs", "timeout"] }
```

- [ ] **Step 2: Write the security-header middleware** — create `crates/knot-server/src/security_headers.rs`:

```rust
//! Sets security headers on every response. CSP is enforced; it is intentionally
//! permissive only where the SPA requires it (inline styles from Tiptap/Excalidraw).
//! HSTS is emitted only when cookies are Secure (i.e. served over TLS).

use axum::{
    extract::{Request, State},
    http::{HeaderValue, header},
    middleware::Next,
    response::Response,
};

// `connect-src` includes ws/wss for the same-origin collab socket; `img-src`
// allows data:/blob: for editor/board images; `worker-src blob:` for Yjs/Excalidraw.
const CSP: &str = "default-src 'self'; \
base-uri 'self'; object-src 'none'; frame-ancestors 'none'; \
img-src 'self' data: blob:; font-src 'self' data:; \
style-src 'self' 'unsafe-inline'; script-src 'self'; \
connect-src 'self' ws: wss:; worker-src 'self' blob:";

pub async fn set_security_headers(
    State(cookie_secure): State<bool>,
    req: Request,
    next: Next,
) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert("content-security-policy", HeaderValue::from_static(CSP));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("strict-origin-when-cross-origin"));
    if cookie_secure {
        h.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        );
    }
    res
}
```

- [ ] **Step 3: Wire the layers in `router_with_state`** — in `lib.rs`, add `mod security_headers;` next to the other module declarations.

**CRITICAL — do not apply the request timeout or body limit to the `/collab/*` WebSocket routes** (a 30s timeout / 2 MB body limit would break long-lived collab sockets). Restructure so the timeout + body-limit wrap only the non-WS routes, while the session-loader, security headers, trace, and metrics stay global. Currently the router is built as one `Router` with the two `/collab/*` routes merged in; split them:

```rust
    // WS routes: NO timeout / body-limit (long-lived, streamed).
    let collab = Router::new()
        .route("/collab/doc/:doc_id", get(collab_upgrade))
        .route("/collab/board/:board_id", get(collab_board_upgrade));

    // Everything else: request timeout + body-size limit.
    let api_and_pages = Router::new()
        .merge(routes::health::router())
        .merge(routes::auth::router())
        .merge(routes::public::router())
        .merge(routes::api::router(state.clone()))
        .fallback_service(spa)
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(tower_http::timeout::TimeoutLayer::new(std::time::Duration::from_secs(30)));

    let mut r = collab.merge(api_and_pages);

    if let Some(deps) = state.session_deps() {
        r = r.layer(axum::middleware::from_fn_with_state(deps, auth::session_loader_mw));
    }

    r.layer(axum::middleware::from_fn_with_state(
        state.cookie_secure,
        crate::security_headers::set_security_headers,
    ))
    .layer(tower_http::trace::TraceLayer::new_for_http())
    .layer(axum::middleware::from_fn(crate::metrics::record))
    .with_state(state)
```
Notes for the implementer:
- Keep the existing `spa`/`index_path` setup above this block unchanged; only the router assembly changes.
- `DefaultBodyLimit` only affects body **extractors** (Json/String/Bytes/Multipart). The blob upload handler takes the raw `Request` and streams via `multer` (its own 10 MB `SizeLimit`), so it is **unaffected** — verify by reading `blobs.rs` upload. The import endpoint (`routes/api/export_import.rs`): read it — if it accepts a body via an extractor and expects payloads >2 MB, add a per-route `DefaultBodyLimit::max(<larger>)` layer on just that route; otherwise leave global. State which you did.
- The session-loader must remain global (collab routes read `AuthContext`); it stays applied after the merge, as shown.

- [ ] **Step 4: Header presence test** — extend `http_hardening_integration.rs`:
```rust
// GET /api/healthz (no auth) → assert security headers present.
assert_eq!(r.headers()["content-security-policy"], CSP_EXPECTED /* the string */);
assert_eq!(r.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
assert_eq!(r.headers()[header::X_FRAME_OPTIONS], "DENY");
```
Run: `cargo nextest run -p knot-server --test http_hardening_integration` → PASS.

- [ ] **Step 5: CSP validation via e2e (the real gate)** — run the FULL Playwright suite, which exercises the editor, boards, comments, public docs, mermaid, uploads, history, import/export:
```bash
cd e2e && pnpm playwright test
```
Expected: all green with CSP enforced. If any spec fails due to a CSP violation (check the browser console / failure), relax the CSP **minimally** and document the reason inline in `CSP` (e.g. add `'wasm-unsafe-eval'` to `script-src` only if a feature needs wasm; add a host to `connect-src` only if a real external call exists). Re-run until green. Report exactly which (if any) relaxations were required and why. Do NOT broaden `script-src` to `'unsafe-inline'`/`'unsafe-eval'` unless a specific feature provably requires it.

- [ ] **Step 6: clippy + commit**
```bash
cargo clippy -p knot-server --all-targets -- -D warnings
git add Cargo.toml crates/knot-server/src/security_headers.rs crates/knot-server/src/lib.rs crates/knot-server/tests/http_hardening_integration.rs
git commit -m "feat(http): body limit, request timeout, and security headers incl. enforced CSP"
```

---

## Task 6: Full verification

- [ ] **Step 1:** `cargo nextest run -p knot-server -p knot-config` → all pass (incl. `http_hardening_integration`, protocol unit tests).
- [ ] **Step 2:** `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- [ ] **Step 3:** `cd e2e && pnpm playwright test` → all pass with CSP enforced. (Also run `cd web && pnpm test && pnpm tsc --noEmit` — unaffected, but confirm.)
- [ ] **Step 4: Manual smoke** — `make dev`, open the editor + a board + a mermaid block + a public doc; confirm no CSP console errors and uploads download (not inline) for SVG.

---

## Self-Review notes
- **Spec coverage (Branch 1):** body limit + timeout + security headers/CSP (Task 5) ✓; blob nosniff/attachment/SVG coercion (Task 2) ✓; WS frame cap (Task 3) ✓; varuint overflow (Task 1) ✓; `cookie_secure` flag introduced here for HSTS, reused by Branch 2 (Task 4) ✓.
- **Cross-branch note:** `cookie_secure` config + AppState field land here; Branch 2 switches `cookies.rs` to consume it and drops the `base_url`-scheme derivation.
- **Naming consistency:** `cookie_secure` (config + AppState + middleware state), `safe_inline_content_type`, `MAX_WS_MESSAGE_BYTES`, `set_security_headers` used consistently.
- **Risk:** CSP is the only app-breakage risk; gated on a green full Playwright run (Task 5 Step 5) with documented minimal relaxations.
